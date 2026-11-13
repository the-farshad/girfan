# girfan

Your bank accounts, credit cards and spending in one place, on your own computer. Bank data
comes through [Plaid](https://plaid.com); everything girfan keeps is encrypted on disk. It has a
dashboard in the terminal and the same dashboard as a local web page.

*Girfan* (گیرفان) is Kurdish for "pocket".

## What it shows

- Balances of every account, and your net worth
- Credit cards: balance, limit used, statement, minimum payment, due date
- Spending per month, and this month by category
- Transactions across all banks, with search
- Alerts: card payments due soon or overdue, cards near their limit, large charges, banks
  that need you to log in again or have not synced lately

## Security

- Read-only: girfan asks Plaid only for transactions and, where offered, card details. It
  never asks for full account numbers and cannot move money.
- Your bank login happens on your bank's page or in Plaid's window; girfan never sees it.
- The database is opened in memory only and saved encrypted (AES-256-GCM). Plaid keys, bank
  tokens and the database key are in a file encrypted with your passphrase (scrypt). Files
  are readable by you only.
- The web page listens on 127.0.0.1 only, while it runs, and needs a one-time address. It
  loads nothing from outside, except Plaid's script on the page that links a bank.

## Install

```sh
cargo build --release
cp target/release/girfan ~/.local/bin/
```

## Use

Create a free account at [dashboard.plaid.com](https://dashboard.plaid.com). Its Trial plan
allows 10 linked banks, and a removed bank does not give its slot back, so try everything in
Sandbox (fake data) first.

```sh
girfan --sandbox keys          # client_id and Sandbox secret, from the Plaid dashboard
girfan --sandbox link --test   # link Plaid's test bank
girfan --sandbox               # the dashboard
girfan --sandbox web           # the dashboard in your browser

girfan keys                    # client_id and Production secret
girfan search chase            # is a bank supported? (uses none of your links)
girfan link                    # link a bank in your browser
girfan sync                    # fetch new data
girfan                         # the dashboard
```

Other commands: `tx` (search transactions), `import` (OFX or QFX statement files, such as
Apple Card's export from the Wallet app), `banks`, `unlink`, and `set big-charge` /
`set due-days` for the alerts. Run `girfan help` for details.

Data is kept in `~/.local/share/girfan/` (or `$XDG_DATA_HOME/girfan/`). For unattended syncs,
`GIRFAN_PASSPHRASE_FILE` names a file holding the passphrase; anyone who can read that file
can unlock your secrets.

## Tests

```sh
cargo test
```

The tests need no network and no Plaid keys.

## License

GNU General Public License v3.0; see [LICENSE](LICENSE).
