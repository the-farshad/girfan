//! Where girfan keeps its data, all of it encrypted.
//!
//! The secrets (the Plaid keys, each bank's access token, and the database key) are one JSON
//! object per environment, in a file encrypted with a key derived from your passphrase
//! (scrypt, then AES-256-GCM). The database is SQLite, opened in memory only and saved
//! encrypted with AES-256-GCM.

use crate::util::{hex, random, seal, unhex, unseal, write_private};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::time::SystemTime;

pub const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS items (
  item_id TEXT PRIMARY KEY, institution_id TEXT, institution TEXT, cursor TEXT,
  added TEXT, synced TEXT, error TEXT);
CREATE TABLE IF NOT EXISTS accounts (
  account_id TEXT PRIMARY KEY, item_id TEXT, institution TEXT, name TEXT, mask TEXT,
  type TEXT, subtype TEXT, current REAL, available REAL, credit_limit REAL, currency TEXT,
  updated TEXT);
CREATE TABLE IF NOT EXISTS transactions (
  transaction_id TEXT PRIMARY KEY, account_id TEXT, date TEXT, name TEXT, merchant TEXT,
  amount REAL, currency TEXT, category TEXT, detail TEXT, pending INTEGER);
CREATE TABLE IF NOT EXISTS cards (
  account_id TEXT PRIMARY KEY, statement_balance REAL, statement_date TEXT,
  minimum_payment REAL, due_date TEXT, last_payment REAL, last_payment_date TEXT,
  overdue INTEGER, updated TEXT);
CREATE TABLE IF NOT EXISTS settings (key TEXT PRIMARY KEY, value TEXT);
CREATE INDEX IF NOT EXISTS transactions_date ON transactions (date);
";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Env {
    Sandbox,
    Production,
}

impl Env {
    pub fn host(self) -> &'static str {
        match self {
            Env::Sandbox => "https://sandbox.plaid.com",
            Env::Production => "https://production.plaid.com",
        }
    }
    /// "--sandbox " for messages that show a command.
    pub fn flag(self) -> &'static str {
        if self == Env::Sandbox {
            "--sandbox "
        } else {
            ""
        }
    }
    fn suffix(self) -> &'static str {
        if self == Env::Sandbox { "-sandbox" } else { "" }
    }
}

/// Where the passphrase comes from.
pub enum Pass {
    Ask,
    File(PathBuf),
    #[allow(dead_code)] // used by the tests
    Given(String),
}

/// The environment, the data folder, and the passphrase source of one run.
pub struct Ctx {
    pub env: Env,
    pub dir: PathBuf,
    pub pass: Pass,
}

impl Ctx {
    /// From $XDG_DATA_HOME (or ~/.local/share) and $GIRFAN_PASSPHRASE_FILE.
    pub fn from_env(env: Env) -> Self {
        let base = std::env::var_os("XDG_DATA_HOME")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/share")
            });
        let pass = match std::env::var_os("GIRFAN_PASSPHRASE_FILE").filter(|v| !v.is_empty()) {
            Some(p) => Pass::File(p.into()),
            None => Pass::Ask,
        };
        Ctx {
            env,
            dir: base.join("girfan"),
            pass,
        }
    }

    fn ask(&self, prompt: &str) -> Result<String, String> {
        match &self.pass {
            Pass::Given(p) => Ok(p.clone()),
            Pass::File(path) => fs::read_to_string(path)
                .map(|s| s.trim_end_matches(['\n', '\r']).to_string())
                .map_err(|e| format!("cannot read GIRFAN_PASSPHRASE_FILE: {e}")),
            Pass::Ask => rpassword::prompt_password(prompt).map_err(|_| {
                "the secrets are locked with a passphrase; run this in a terminal".to_string()
            }),
        }
    }

    fn new_passphrase(&self) -> Result<String, String> {
        let min = if self.env == Env::Sandbox { 0 } else { 12 };
        if !matches!(self.pass, Pass::Ask) {
            let pw = self.ask("")?;
            return if pw.chars().count() >= min {
                Ok(pw)
            } else {
                Err(format!("the passphrase needs at least {min} characters"))
            };
        }
        eprintln!(
            "girfan: your secrets go in an encrypted file, unlocked with a passphrase you choose now."
        );
        if self.env == Env::Sandbox {
            eprintln!("Sandbox data is fake: you may press Enter for no passphrase.");
        }
        loop {
            let pw = self.ask("new passphrase: ")?;
            if pw.chars().count() < min {
                eprintln!("use at least {min} characters for real data");
            } else if self.ask("the same again: ")? == pw {
                return Ok(pw);
            } else {
                eprintln!("they differ; try again");
            }
        }
    }
}

#[derive(Serialize, Deserialize, Clone)]
pub struct Keys {
    pub client_id: String,
    pub secret: String,
}

#[derive(Serialize, Deserialize, Default)]
pub struct SecretData {
    #[serde(default)]
    pub keys: Option<Keys>,
    #[serde(default)]
    pub tokens: BTreeMap<String, String>,
    #[serde(default)]
    pub db_key: Option<String>,
}

const SECRETS_MAGIC: &[u8] = b"GIRFANS1";
const DB_MAGIC: &[u8] = b"GIRFAND1";

fn kdf(salt: &[u8], passphrase: &str) -> [u8; 32] {
    let mut key = [0u8; 32];
    let params = scrypt::Params::new(15, 8, 1).expect("valid scrypt settings");
    scrypt::scrypt(passphrase.as_bytes(), salt, &params, &mut key).expect("a 32-byte key");
    key
}

pub struct Secrets {
    path: PathBuf,
    salt: [u8; 16],
    key: Option<[u8; 32]>,
    pub data: SecretData,
}

impl Secrets {
    pub fn load(ctx: &Ctx) -> Result<Self, String> {
        let path = ctx.dir.join(format!("secrets{}.enc", ctx.env.suffix()));
        if !path.exists() {
            return Ok(Secrets {
                path,
                salt: random(),
                key: None,
                data: SecretData::default(),
            });
        }
        let blob = fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        if !blob.starts_with(SECRETS_MAGIC) || blob.len() < 8 + 16 + 12 + 16 {
            return Err(format!("{} is not a girfan secrets file", path.display()));
        }
        let salt: [u8; 16] = blob[8..24].try_into().expect("16 bytes");
        let mut tries: Vec<Option<String>> = vec![None, None, None];
        if ctx.env == Env::Sandbox {
            tries.insert(0, Some(String::new())); // Sandbox may have no passphrase
        }
        for t in tries {
            let asked = t.is_none();
            let pw = match t {
                Some(p) => p,
                None => ctx.ask("girfan passphrase: ")?,
            };
            let key = kdf(&salt, &pw);
            if let Some(plain) = unseal(&key, SECRETS_MAGIC, &blob[24..]) {
                let data =
                    serde_json::from_slice(&plain).map_err(|e| format!("damaged secrets: {e}"))?;
                return Ok(Secrets {
                    path,
                    salt,
                    key: Some(key),
                    data,
                });
            }
            if asked {
                eprintln!("girfan: wrong passphrase");
            }
        }
        Err("could not unlock the secrets".into())
    }

    pub fn where_(&self) -> String {
        format!("{} (encrypted with your passphrase)", self.path.display())
    }

    pub fn save(&mut self, ctx: &Ctx) -> Result<(), String> {
        let key = match self.key {
            Some(k) => k,
            None => {
                let pw = ctx.new_passphrase()?;
                self.salt = random();
                let k = kdf(&self.salt, &pw);
                self.key = Some(k);
                k
            }
        };
        let raw = serde_json::to_vec(&self.data).expect("JSON");
        let blob = [
            SECRETS_MAGIC,
            &self.salt[..],
            &seal(&key, SECRETS_MAGIC, &raw),
        ]
        .concat();
        write_private(&self.path, &blob).map_err(|e| format!("{}: {e}", self.path.display()))
    }
}

/// The data of one environment: SQLite in memory, saved encrypted. Changes go through
/// write(), which holds a lock and starts from the latest saved copy, so two girfan commands
/// (say, the web page and a sync) do not overwrite each other's work.
pub struct Database {
    pub conn: Connection,
    path: PathBuf,
    key: [u8; 32],
    stamp: Option<SystemTime>,
}

fn sql(e: rusqlite::Error) -> String {
    format!("database: {e}")
}

impl Database {
    pub fn open(ctx: &Ctx, secrets: &mut Secrets) -> Result<Self, String> {
        let path = ctx.dir.join(format!("girfan{}.db.enc", ctx.env.suffix()));
        let stored = secrets
            .data
            .db_key
            .as_deref()
            .and_then(unhex)
            .and_then(|k| <[u8; 32]>::try_from(k).ok());
        let key = match stored {
            Some(k) => k,
            None if path.exists() => {
                return Err("the database key is missing from the secrets".into());
            }
            None => {
                let k: [u8; 32] = random();
                secrets.data.db_key = Some(hex(&k));
                secrets.save(ctx)?;
                k
            }
        };
        let conn = Connection::open_in_memory().map_err(sql)?;
        let mut db = Database {
            conn,
            path,
            key,
            stamp: None,
        };
        db.read()?;
        Ok(db)
    }

    fn mtime(&self) -> Option<SystemTime> {
        fs::metadata(&self.path).and_then(|m| m.modified()).ok()
    }

    fn read(&mut self) -> Result<(), String> {
        if self.path.exists() {
            let blob = fs::read(&self.path).map_err(|e| format!("{}: {e}", self.path.display()))?;
            if !blob.starts_with(DB_MAGIC) {
                return Err(format!("{} is not a girfan database", self.path.display()));
            }
            let plain = unseal(&self.key, DB_MAGIC, &blob[8..]).ok_or_else(|| {
                format!(
                    "{} could not be decrypted with the stored key",
                    self.path.display()
                )
            })?;
            let n = plain.len();
            self.conn
                .deserialize_read_exact("main", &plain[..], n, false)
                .map_err(sql)?;
            self.stamp = self.mtime();
        }
        self.conn.execute_batch(SCHEMA).map_err(sql)
    }

    /// Reloads the database when another girfan command has saved it since.
    pub fn refresh(&mut self) -> Result<(), String> {
        if self.path.exists() && self.mtime() != self.stamp {
            self.read()
        } else {
            Ok(())
        }
    }

    fn save(&mut self) -> Result<(), String> {
        let data = self.conn.serialize("main").map_err(sql)?;
        let blob = [DB_MAGIC, &seal(&self.key, DB_MAGIC, &data)].concat();
        write_private(&self.path, &blob).map_err(|e| format!("{}: {e}", self.path.display()))?;
        self.stamp = self.mtime();
        Ok(())
    }

    /// Runs F on the latest saved copy under a lock, then saves (also when F fails).
    pub fn write<T>(
        &mut self,
        f: impl FnOnce(&Connection) -> Result<T, String>,
    ) -> Result<T, String> {
        let mut lock_path = self.path.as_os_str().to_owned();
        lock_path.push(".lock");
        fs::create_dir_all(self.path.parent().expect("a folder")).map_err(|e| e.to_string())?;
        let lock = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(&lock_path)
            .map_err(|e| format!("lock: {e}"))?;
        lock.lock().map_err(|e| format!("lock: {e}"))?;
        self.refresh()?;
        let result = f(&self.conn);
        self.save()?;
        result
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static N: AtomicUsize = AtomicUsize::new(0);

    /// A fresh data folder for one test, removed when dropped.
    pub struct TempDir(pub PathBuf);
    impl TempDir {
        pub fn new() -> Self {
            let n = N.fetch_add(1, Ordering::SeqCst);
            let p = std::env::temp_dir().join(format!("girfan-test-{}-{n}", std::process::id()));
            let _ = fs::remove_dir_all(&p);
            fs::create_dir_all(&p).unwrap();
            TempDir(p)
        }
        pub fn ctx(&self, env: Env, pass: &str) -> Ctx {
            Ctx {
                env,
                dir: self.0.join("girfan"),
                pass: Pass::Given(pass.into()),
            }
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    const PW: &str = "correct horse battery staple";

    #[test]
    fn secrets_are_encrypted_and_private() {
        use std::os::unix::fs::PermissionsExt;
        let t = TempDir::new();
        let ctx = t.ctx(Env::Production, PW);
        let mut s = Secrets::load(&ctx).unwrap();
        s.data.keys = Some(Keys {
            client_id: "id-marker".into(),
            secret: "secret-marker".into(),
        });
        s.save(&ctx).unwrap();
        let raw = fs::read(&s.path).unwrap();
        assert!(!raw.windows(6).any(|w| w == b"marker"));
        assert_eq!(
            fs::metadata(&s.path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(&ctx.dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            Secrets::load(&ctx).unwrap().data.keys.unwrap().secret,
            "secret-marker"
        );
    }

    #[test]
    fn wrong_or_short_passphrases_are_refused() {
        let t = TempDir::new();
        let ctx = t.ctx(Env::Production, PW);
        let mut s = Secrets::load(&ctx).unwrap();
        s.save(&ctx).unwrap();
        assert!(Secrets::load(&t.ctx(Env::Production, "not the right one")).is_err());
        let short = TempDir::new();
        assert!(
            Secrets::load(&short.ctx(Env::Production, "short"))
                .unwrap()
                .save(&short.ctx(Env::Production, "short"))
                .is_err()
        );
    }

    #[test]
    fn sandbox_may_have_no_passphrase() {
        let t = TempDir::new();
        let mut s = Secrets::load(&t.ctx(Env::Sandbox, "")).unwrap();
        s.data.db_key = Some("ab".into());
        s.save(&t.ctx(Env::Sandbox, "")).unwrap();
        // opened without asking, even though the passphrase source now says something else
        assert_eq!(
            Secrets::load(&t.ctx(Env::Sandbox, "something else"))
                .unwrap()
                .data
                .db_key
                .as_deref(),
            Some("ab")
        );
    }

    #[test]
    fn database_is_encrypted_reloads_and_merges_two_writers() {
        let t = TempDir::new();
        let ctx = t.ctx(Env::Production, PW);
        let mut secrets = Secrets::load(&ctx).unwrap();
        let mut first = Database::open(&ctx, &mut secrets).unwrap();
        let mut second = Database::open(&ctx, &mut secrets).unwrap();
        first
            .write(|c| {
                c.execute("INSERT INTO settings VALUES ('a', 'Checking-marker')", [])
                    .map_err(sql)
            })
            .unwrap();
        second
            .write(|c| {
                c.execute("INSERT INTO settings VALUES ('b', '2')", [])
                    .map_err(sql)
            })
            .unwrap();
        assert!(
            !fs::read(&first.path)
                .unwrap()
                .windows(15)
                .any(|w| w == b"Checking-marker")
        );
        let again = Database::open(&ctx, &mut Secrets::load(&ctx).unwrap()).unwrap();
        let n: i64 = again
            .conn
            .query_row("SELECT COUNT(*) FROM settings", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 2);
    }
}
