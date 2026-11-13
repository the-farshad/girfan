//! girfan: your bank accounts, cards and spending, kept on your own computer.

mod ofx;
mod plaid;
mod report;
mod store;
mod tui;
mod util;
mod web;

use chrono::{Duration, Local};
use clap::{Parser, Subcommand, ValueEnum};
use plaid::Plaid;
use report::Filter;
use rusqlite::params;
use std::io::{BufRead, IsTerminal};
use std::path::PathBuf;
use std::process::ExitCode;
use store::{Ctx, Database, Env, Keys, Secrets};

#[derive(Parser)]
#[command(
    name = "girfan",
    version,
    about = "Your bank accounts, cards and spending, on your own computer.\n\
                                             Bank data comes through Plaid; everything is stored encrypted here."
)]
struct Cli {
    /// Use Plaid's Sandbox (test data) instead of your real banks
    #[arg(long, global = true)]
    sandbox: bool,
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Show the dashboard in the terminal (the default)
    Dash,
    /// Store your Plaid client_id and secret
    Keys,
    /// Connect a bank (opens a page in your browser)
    Link {
        /// Log in again to a linked bank that asks for it
        #[arg(long, value_name = "BANK")]
        update: Option<String>,
        /// Sandbox only: link Plaid's test bank without a browser
        #[arg(long)]
        test: bool,
        /// Print the address instead of opening a browser
        #[arg(long)]
        no_browser: bool,
    },
    /// Fetch new transactions, balances and card details
    Sync,
    /// List transactions
    Tx {
        /// Words in the description, merchant or category
        words: Vec<String>,
        /// Only the last N days
        #[arg(long)]
        days: Option<i64>,
        /// Only accounts whose bank, name or last 4 digits match
        #[arg(long)]
        account: Option<String>,
        /// At most this many
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },
    /// Show the dashboard in your browser
    Web {
        /// Print the address instead of opening a browser
        #[arg(long)]
        no_browser: bool,
    },
    /// Import OFX or QFX statements (such as Apple Card's export)
    Import {
        #[arg(required = true)]
        files: Vec<PathBuf>,
        /// Account name to show (default: taken from the file)
        #[arg(long)]
        name: Option<String>,
    },
    /// List linked banks and imported accounts
    Banks,
    /// Remove a linked bank (on Plaid's Trial plan this does not free its slot)
    Unlink {
        /// Its name, or the start of its id as `girfan banks` shows it
        bank: String,
    },
    /// Check whether Plaid supports a bank (free; uses none of your links)
    Search {
        #[arg(required = true)]
        name: Vec<String>,
    },
    /// Alert settings: big-charge in dollars (default 500), due-days (default 5)
    Set { key: SetKey, value: f64 },
}

#[derive(Clone, ValueEnum)]
enum SetKey {
    BigCharge,
    DueDays,
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("girfan: {e}");
            ExitCode::FAILURE
        }
    }
}

fn sql(e: rusqlite::Error) -> String {
    format!("database: {e}")
}

fn find_item(db: &Database, reference: &str) -> Result<(String, String), String> {
    let mut st = db
        .conn
        .prepare("SELECT item_id, COALESCE(institution, '') FROM items")
        .map_err(sql)?;
    let rows = st
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
        .map_err(sql)?;
    let want = reference.to_lowercase();
    let mut hits: Vec<(String, String)> = rows
        .filter_map(Result::ok)
        .filter(|(id, inst)| id.starts_with(reference) || inst.to_lowercase().contains(&want))
        .collect();
    match hits.len() {
        1 => Ok(hits.remove(0)),
        0 => Err(format!(
            "no linked bank matches {reference:?}; see: girfan banks"
        )),
        _ => Err(format!(
            "more than one linked bank matches {reference:?}; see: girfan banks"
        )),
    }
}

fn run(cli: Cli) -> Result<(), String> {
    let env = if cli.sandbox {
        Env::Sandbox
    } else {
        Env::Production
    };
    let ctx = Ctx::from_env(env);
    let mut secrets = Secrets::load(&ctx)?;
    let cmd = cli.cmd.unwrap_or(Cmd::Dash);

    if let Cmd::Keys = cmd {
        eprintln!(
            "Your keys are in the Plaid Dashboard under Developers > Keys (use the {} secret). Typing does not show.\n\
                   They will be kept in {}.",
            if env == Env::Sandbox {
                "Sandbox"
            } else {
                "Production"
            },
            secrets.where_()
        );
        let ask = |p: &str| {
            rpassword::prompt_password(p)
                .map(|s| s.trim().to_string())
                .map_err(|_| "run this in a terminal".to_string())
        };
        let keys = Keys {
            client_id: ask("client_id: ")?,
            secret: ask("secret: ")?,
        };
        if keys.client_id.is_empty() || keys.secret.is_empty() {
            return Err("nothing stored".into());
        }
        // a harmless call, to check the keys before keeping them
        Plaid::with_keys(env, keys.clone())
            .link_token(None)
            .map_err(|e| format!("Plaid did not accept these keys ({e}); nothing stored"))?;
        secrets.data.keys = Some(keys);
        secrets.save(&ctx)?;
        eprintln!("girfan: keys accepted by Plaid and stored");
        return Ok(());
    }

    let mut db = Database::open(&ctx, &mut secrets)?;
    let today = Local::now().date_naive();
    let style = tui::Style::new(std::io::stdout().is_terminal());
    let mut log = |m: String| eprintln!("{m}");
    match cmd {
        Cmd::Dash | Cmd::Keys => print!(
            "{}",
            tui::dashboard(&db.conn, today, &style, tui::width()).map_err(sql)?
        ),
        Cmd::Tx {
            words,
            days,
            account,
            limit,
        } => {
            let filter = Filter {
                text: Some(words.join(" ")),
                since: days
                    .filter(|d| *d > 0)
                    .map(|d| (today - Duration::days(d)).to_string()),
                account,
                min_amount: None,
                limit,
            };
            let rows = report::transactions(&db.conn, &filter).map_err(sql)?;
            println!(
                "{}",
                tui::transaction_lines(&rows, &style, tui::width()).join("\n")
            );
        }
        Cmd::Web { no_browser } => web::serve(&ctx, &mut secrets, &mut db, "/", None, !no_browser)?,
        Cmd::Link {
            update,
            test,
            no_browser,
        } => {
            let plaid = Plaid::new(env, &secrets)?; // stops here with a hint when there are no keys yet
            if test {
                if env != Env::Sandbox {
                    return Err("--test works with --sandbox only".into());
                }
                let name = plaid.sandbox_item(&ctx, &mut db, &mut secrets)?;
                eprintln!("girfan: linked {name} (Sandbox)");
                plaid.sync(&mut db, &secrets, &mut log)?;
            } else {
                let update = update
                    .map(|u| find_item(&db, &u).map(|(id, _)| id))
                    .transpose()?;
                web::serve(&ctx, &mut secrets, &mut db, "/link", update, !no_browser)?;
            }
        }
        Cmd::Sync => Plaid::new(env, &secrets)?.sync(&mut db, &secrets, &mut log)?,
        Cmd::Import { files, name } => {
            for f in files {
                let (shown, new, total) = ofx::import_file(&mut db, &f, name.as_deref())?;
                eprintln!(
                    "girfan: {shown}: {new} new of {total} transactions in {}",
                    f.display()
                );
            }
        }
        Cmd::Banks => {
            let mut st = db.conn.prepare("SELECT item_id, COALESCE(institution, ''), synced, error FROM items ORDER BY institution").map_err(sql)?;
            let items: Vec<(String, String, Option<String>, Option<String>)> = st
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
                .map_err(sql)?
                .filter_map(Result::ok)
                .collect();
            for (id, inst, synced, error) in &items {
                let state = match (error, synced) {
                    (Some(e), _) => format!("error {e}"),
                    (None, Some(s)) => format!(
                        "synced {}",
                        s.replace('T', " ").chars().take(16).collect::<String>()
                    ),
                    _ => "not synced yet".into(),
                };
                println!("{:<8}  {inst:<34} {state}", &id[..8.min(id.len())]);
            }
            let mut st = db
                .conn
                .prepare("SELECT name, mask FROM accounts WHERE item_id IS NULL ORDER BY name")
                .map_err(sql)?;
            for row in st
                .query_map([], |r| {
                    Ok((
                        r.get::<_, Option<String>>(0)?,
                        r.get::<_, Option<String>>(1)?,
                    ))
                })
                .map_err(sql)?
            {
                let (name, mask) = row.map_err(sql)?;
                let shown =
                    name.unwrap_or_default() + &mask.map(|m| format!(" ··{m}")).unwrap_or_default();
                println!("{:<8}  {shown:<34} imported from statements", "file");
            }
            if items.is_empty() {
                eprintln!(
                    "girfan: no linked banks yet; run: girfan {}link",
                    env.flag()
                );
            }
        }
        Cmd::Unlink { bank } => {
            let (id, inst) = find_item(&db, &bank)?;
            eprint!(
                "Remove {inst}? girfan loses access to it and forgets its data here. On Plaid's Trial plan the slot \
                     is not given back. [y/N] "
            );
            let mut answer = String::new();
            std::io::stdin()
                .lock()
                .read_line(&mut answer)
                .map_err(|e| e.to_string())?;
            if answer.trim().eq_ignore_ascii_case("y") {
                Plaid::new(env, &secrets)?.remove(&ctx, &mut db, &mut secrets, &id)?;
                eprintln!("girfan: removed {inst}");
            }
        }
        Cmd::Search { name } => {
            let hits = Plaid::new(env, &secrets)?
                .search(&name.join(" "))
                .map_err(|e| e.to_string())?;
            for (n, id, oauth) in &hits {
                println!(
                    "{n:<50} {id:<14} {}",
                    if *oauth {
                        "own login page (OAuth)"
                    } else {
                        "login inside Plaid"
                    }
                );
            }
            if hits.is_empty() {
                eprintln!("girfan: no institution with transactions found under that name");
            }
        }
        Cmd::Set { key, value } => {
            let key = match key {
                SetKey::BigCharge => "big_charge",
                SetKey::DueDays => "due_days",
            };
            db.write(|c| {
                c.execute(
                    "INSERT OR REPLACE INTO settings (key, value) VALUES (?1, ?2)",
                    params![key, value.to_string()],
                )
                .map_err(sql)
            })?;
            eprintln!("girfan: {} set to {value}", key.replace('_', "-"));
        }
    }
    Ok(())
}
