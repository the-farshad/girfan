//! OFX and QFX statement files (Apple Card's export, or any bank's download).
//!
//! Both the older SGML form (tags without closing tags) and the XML form are read. Amounts are
//! stored the way Plaid gives them, positive for money going out, so OFX amounts (negative for
//! money going out) change sign.

use crate::plaid::now;
use crate::store::Database;
use crate::util::digest;
use regex::Regex;
use rusqlite::params;
use std::collections::HashMap;
use std::path::Path;
use std::sync::OnceLock;

pub struct Txn {
    pub fitid: String,
    pub date: String,
    pub amount: f64,
    pub name: String,
    pub kind: String,
}

pub struct Statement {
    pub card: bool,
    pub account: String,
    pub org: String,
    pub balance: Option<f64>,
    pub txns: Vec<Txn>,
}

fn re(pattern: &'static str, cell: &'static OnceLock<Regex>) -> &'static Regex {
    cell.get_or_init(|| Regex::new(pattern).expect("a valid pattern"))
}

fn tags(block: &str) -> HashMap<String, String> {
    static TAG: OnceLock<Regex> = OnceLock::new();
    re(r"<([A-Za-z0-9.]+)>([^<\r\n]*)", &TAG)
        .captures_iter(block)
        .filter(|c| !c[2].trim().is_empty())
        .map(|c| (c[1].to_uppercase(), c[2].trim().to_string()))
        .collect()
}

fn date(value: &str) -> Option<String> {
    let d = value.get(..8)?;
    d.bytes()
        .all(|b| b.is_ascii_digit())
        .then(|| format!("{}-{}-{}", &d[..4], &d[4..6], &d[6..8]))
}

pub fn parse(text: &str) -> Statement {
    static CARD: OnceLock<Regex> = OnceLock::new();
    static ACCT: OnceLock<Regex> = OnceLock::new();
    static ORG: OnceLock<Regex> = OnceLock::new();
    static BAL: OnceLock<Regex> = OnceLock::new();
    static TRN: OnceLock<Regex> = OnceLock::new();
    let card = re(r"(?i)<CCSTMTRS>", &CARD).is_match(text);
    let one = |r: &Regex| {
        r.captures(text)
            .map(|c| c[1].trim().to_string())
            .unwrap_or_default()
    };
    let account = one(re(r"(?i)<ACCTID>([^<\r\n]+)", &ACCT));
    let org = one(re(r"(?i)<ORG>([^<\r\n]+)", &ORG));
    let balance = re(r"(?is)<LEDGERBAL>(.*?)</LEDGERBAL>", &BAL)
        .captures(text)
        .and_then(|c| {
            tags(&c[1])
                .get("BALAMT")
                .and_then(|v| v.parse::<f64>().ok())
        })
        .map(|b| if card { -b } else { b }); // a card's balance is what you owe
    let txns = re(r"(?is)<STMTTRN>(.*?)</STMTTRN>", &TRN)
        .captures_iter(text)
        .filter_map(|c| {
            let t = tags(&c[1]);
            let amount = t.get("TRNAMT")?.parse::<f64>().ok()?;
            Some(Txn {
                fitid: t.get("FITID").cloned().unwrap_or_else(|| digest(&c[1], 40)),
                date: date(t.get("DTPOSTED")?)?,
                amount: -amount,
                name: t
                    .get("NAME")
                    .or(t.get("MEMO"))
                    .or(t.get("PAYEE"))
                    .cloned()
                    .unwrap_or_else(|| "?".into()),
                kind: t
                    .get("TRNTYPE")
                    .map(|s| s.to_uppercase())
                    .unwrap_or_default(),
            })
        })
        .collect();
    Statement {
        card,
        account,
        org,
        balance,
        txns,
    }
}

/// Adds an OFX statement to the database: (account name, new transactions, transactions).
pub fn import_file(
    db: &mut Database,
    path: &Path,
    name: Option<&str>,
) -> Result<(String, usize, usize), String> {
    let raw = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let st = parse(&String::from_utf8_lossy(&raw));
    if st.txns.is_empty() && st.balance.is_none() {
        return Err(format!(
            "{}: no OFX transactions or balance found",
            path.display()
        ));
    }
    let account_id = format!("ofx:{}", digest(&format!("{}|{}", st.org, st.account), 16));
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mask: String = st
        .account
        .chars()
        .rev()
        .take(4)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    db.write(|c| {
        let old: Option<String> = c.query_row("SELECT name FROM accounts WHERE account_id = ?1", [&account_id], |r| r.get(0)).ok();
        // keep an earlier name unless a new one is given
        let shown = name.map(String::from).or(old).or_else(|| Some(st.org.clone()).filter(|o| !o.is_empty())).unwrap_or(stem);
        c.execute("INSERT OR REPLACE INTO accounts (account_id, item_id, institution, name, mask, type, current, updated) \
                   VALUES (?1, NULL, ?2, ?2, ?3, ?4, ?5, ?6)",
                  params![account_id, shown, Some(mask).filter(|m| !m.is_empty()), if st.card { "credit" } else { "depository" },
                          st.balance, now()])
            .map_err(|e| e.to_string())?;
        let mut added = 0;
        for t in &st.txns {
            let card_payment = st.card && t.amount < 0.0 && (t.kind.contains("PAYMENT") || t.name.to_lowercase().contains("payment"));
            let (category, detail) = if card_payment { (Some("LOAN_PAYMENTS"), Some(crate::report::CARD_PAYMENT)) } else { (None, None) };
            added += c.execute("INSERT OR IGNORE INTO transactions (transaction_id, account_id, date, name, amount, category, \
                                detail, pending) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 0)",
                               params![format!("ofx:{}", digest(&format!("{account_id}|{}", t.fitid), 20)), account_id, t.date,
                                       t.name, t.amount, category, detail])
                .map_err(|e| e.to_string())?;
        }
        Ok((shown, added, st.txns.len()))
    })
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::store::tests::TempDir;
    use crate::store::{Env, Secrets};

    pub const CARD_OFX: &str = "OFXHEADER:100
DATA:OFXSGML
<OFX><SIGNONMSGSRSV1><SONRS><FI><ORG>Apple Card<FID>1</FI></SONRS></SIGNONMSGSRSV1>
<CREDITCARDMSGSRSV1><CCSTMTTRNRS><CCSTMTRS><CURDEF>USD<CCACCTFROM><ACCTID>123456789012</CCACCTFROM>
<BANKTRANLIST>
<STMTTRN><TRNTYPE>DEBIT<DTPOSTED>20261102120000<TRNAMT>-42.50<FITID>A1<NAME>Corner Grocery</STMTTRN>
<STMTTRN><TRNTYPE>PAYMENT<DTPOSTED>20261105<TRNAMT>300.00<FITID>A2<NAME>ACH Deposit Payment</STMTTRN>
</BANKTRANLIST>
<LEDGERBAL><BALAMT>-512.25<DTASOF>20261130</LEDGERBAL>
</CCSTMTRS></CCSTMTTRNRS></CREDITCARDMSGSRSV1></OFX>
";
    const BANK_OFX: &str = "<?xml version=\"1.0\"?><OFX><BANKMSGSRSV1><STMTTRNRS><STMTRS><BANKACCTFROM><ACCTID>9876</ACCTID></BANKACCTFROM>
<BANKTRANLIST><STMTTRN><TRNTYPE>CREDIT</TRNTYPE><DTPOSTED>20261101</DTPOSTED><TRNAMT>1500.00</TRNAMT><FITID>B1</FITID><NAME>Payroll</NAME></STMTTRN></BANKTRANLIST>
<LEDGERBAL><BALAMT>2400.10</BALAMT><DTASOF>20261101</DTASOF></LEDGERBAL></STMTRS></STMTTRNRS></BANKMSGSRSV1></OFX>";

    #[test]
    fn card_statement_in_sgml() {
        let st = parse(CARD_OFX);
        assert!(st.card);
        assert_eq!(st.balance, Some(512.25)); // owed
        assert_eq!(
            st.txns.iter().map(|t| t.amount).collect::<Vec<_>>(),
            vec![42.5, -300.0]
        );
        assert_eq!(st.txns[0].date, "2026-11-02");
    }

    #[test]
    fn bank_statement_in_xml() {
        let st = parse(BANK_OFX);
        assert!(!st.card);
        assert_eq!(st.balance, Some(2400.10));
        assert_eq!(st.txns[0].amount, -1500.0); // money in
    }

    #[test]
    fn import_is_idempotent_and_marks_card_payments() {
        let t = TempDir::new();
        let ctx = t.ctx(Env::Production, "correct horse battery staple");
        let mut db = Database::open(&ctx, &mut Secrets::load(&ctx).unwrap()).unwrap();
        let file = t.0.join("apple.ofx");
        std::fs::write(&file, CARD_OFX).unwrap();
        assert_eq!(
            import_file(&mut db, &file, None).unwrap(),
            ("Apple Card".into(), 2, 2)
        );
        assert_eq!(
            import_file(&mut db, &file, None).unwrap(),
            ("Apple Card".into(), 0, 2)
        );
        let detail: String = db
            .conn
            .query_row(
                "SELECT detail FROM transactions WHERE amount < 0",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(detail, crate::report::CARD_PAYMENT);
        let mask: String = db
            .conn
            .query_row("SELECT mask FROM accounts", [], |r| r.get(0))
            .unwrap();
        assert_eq!(mask, "9012");
    }
}
