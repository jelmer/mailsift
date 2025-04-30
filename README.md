# mailsift

A tool that watches your email and automatically pulls out the
useful structured bits: calendar events, bills, parcels, receipts,
tickets, subscriptions. The idea is that your inbox already
contains most of the data you care about (flight times, parcel
tracking numbers, invoice due dates, restaurant bookings) and a
small program can lift that data out into proper files and feeds
so you don't have to.

Concretely, for each incoming message mailsift runs a set of small
per-vendor extractor scripts. Each extractor reads the raw RFC822
on stdin and writes typed artifact files into a per-run tempdir:

| Suffix             | What it is                                                                          |
|--------------------|-------------------------------------------------------------------------------------|
| `.event.ics`       | iCalendar event (parsed and re-emitted via the [`icalendar`] crate).                |
| `.reservation.json`| schema.org reservation (Flight/Train/Bus/Lodging/Event/FoodEstablishment). Converted to a single VEVENT. |
| `.bill.json`       | Loosely schema.org `Invoice`-shaped record (payee, invoice number, due date, ...).  |
| `.parcel.json`     | schema.org `ParcelDelivery`-shaped record (merged across status-update mails).      |
| `.receipt.json`    | Loosely schema.org `Order`-shaped record (merchant, order number, date, ...).       |
| `.ticket.<ext>`    | Opaque ticket / boarding pass blob (`pdf`, `pkpass`, image formats).                |

Events go to a CalDAV inbox calendar or to a local `<UID>.ics` directory.
Bills, parcels, receipts and tickets get filed under year-keyed local
directories (parcels are flat, keyed by tracking number, since they're
merged across messages as the parcel progresses).

Extraction is best-effort: failed extractors log and the next message
continues.

## Install

```sh
cargo install --path .
```

The build needs a C toolchain (for `aws-lc-rs`) and, by default, a
system GSSAPI library (MIT Kerberos or Heimdal). To build without
Kerberos:

```sh
cargo install --path . --no-default-features
```

The `gssapi` Cargo feature gates SASL `GSSAPI` for IMAP and HTTP
`Negotiate` for CalDAV. Both fall back gracefully; basic auth still
works.

## Configure

mailsift looks for `$XDG_CONFIG_HOME/mailsift/config.toml`
(typically `~/.config/mailsift/config.toml`) automatically. Pass
`--config <path>` to override. See `config.example.toml` for the
shape; every key is optional.

A minimal config:

```toml
extractors_dir = "/etc/mailsift/extractors"
bills_dir      = "/home/jelmer/Documents/bills"
parcels_dir    = "/home/jelmer/Documents/parcels"
receipts_dir   = "/home/jelmer/Documents/receipts"
tickets_dir    = "/home/jelmer/Documents/tickets"

[caldav]
url           = "https://jelmer@cal.example.org/dav/jelmer/inbox/"
password_file = "/etc/mailsift/caldav.pass"
```

Omit `password_file` (and `user`) to authenticate via Kerberos when the
`gssapi` feature is built in. The username may also be embedded in the
URL's userinfo (`https://user@host/...`); passwords in URLs are not
accepted.

## Run

Three modes:

### `replay`: single message from a file

```sh
mailsift replay /path/to/message.eml
mailsift replay - < message.eml          # stdin
```

Useful for testing extractors against a saved message.

### `imap-scan`: walk an IMAP mailbox

```sh
mailsift imap-scan imaps://jelmer@mail.example.org/INBOX \
    --password-file ~/.config/mailsift/imap.pass \
    --since 01-Jan-2026 --limit 200
```

The URL is the whole connection spec: scheme, optional user, host,
optional port, optional mailbox path. With the `gssapi` feature, omit
`--password-file` to authenticate via Kerberos from the caller's
credential cache. Without a user in the URL the current OS user is
used. Selects the mailbox **read-only**: no flags set, nothing
expunged.

For Gmail (and other XOAUTH2 providers), pass a short-lived OAuth2
bearer token via `--oauth2-token-file` instead of `--password-file`:

```sh
mailsift imap-scan imaps://you@imap.gmail.com/INBOX \
    --oauth2-token-file ~/.cache/mailsift/gmail.token \
    --since 01-Jan-2026
```

The token file must contain just the access token (trailing newline is
trimmed). Obtain one however you like; `oauth2l fetch --type=bearer
--scope=https://mail.google.com/ --output_format=bare > gmail.token`
works for personal accounts; for workspace accounts use a service
account with domain-wide delegation. Gmail access tokens expire after
~1 hour, so refresh before each run.

A progress bar shows scan progress when stderr is a TTY; one summary
line per message names the UID, extractor, and what was extracted:

```
INFO event updated target=/home/jelmer/.../flight-ezy2521@mailsift.ics
INFO extracted from UID 1234: easyjet=2 events
```

Add `--watch` to stay connected after the initial scan and process new
messages as they arrive (IMAP IDLE, RFC 2177). The same connection is
reused; on transport errors it reconnects with exponential backoff
(1, 2, 4, ..., 60 s). `--limit` then applies only to the initial
backfill; once watching, every new UID is processed. Ctrl-C exits
cleanly (within the IDLE keepalive window, currently 5 minutes).

```sh
mailsift imap-scan imaps://jelmer@mail.example.org/INBOX \
    --password-file ~/.config/mailsift/imap.pass --watch
```

Watch refuses to continue if the mailbox's `UIDVALIDITY` changes
between reconnects (server restored from backup or renumbered the
mailbox); restart manually in that case.

### `milter`: Postfix milter

```sh
mailsift milter --socket unix:/run/mailsift/milter.sock
```

Listens for milter calls and runs the pipeline at end-of-message. Always
returns `Continue`; extraction failures never block mail delivery. A
wall-clock deadline (default 20 s) caps each message; if exceeded the
mail is accepted without extraction.

The milter front-end can't enforce extractor-level `require_dkim`
constraints (it sees mail before the local MTA's DKIM check has run), so
it skips that check. Use `replay`/`imap-scan` for retroactive runs that
do want DKIM enforcement.

## Extractors

Each extractor is a pair: a YAML manifest and an executable script.
mailsift discovers them by scanning the configured `extractors_dir`
for `*.yaml`.

A manifest:

```yaml
name: easyjet
order: 50
from_domains:
  - easyjet.com
  - "*.easyjet.com"
subject_regex: "(?i)easyJet booking reference"
requires:
  - html
require_dkim:
  - easyjet.com
```

`require_dkim` is enforced via the topmost `Authentication-Results:`
header. `from_domains` / `subject_regex` / `requires` are recorded but
not yet used for dispatch; every applicable extractor runs against
every message today.

Each script receives the raw RFC822 on stdin, runs in a fresh tempdir,
and writes named artifact files into its cwd. Python extractors can use
the helper at `extractors/_lib/mailsift_extractor.py`; others just
parse the message themselves. Exit 0 means "done, look at my output";
non-zero means "I failed, skip me".

## Development

```sh
cargo test                                  # unit + integration
cargo test --no-default-features            # without gssapi
cargo clippy --all-targets
cargo fmt
```

Integration tests in `tests/` replay corpus messages through the full
pipeline and compare the resulting `.ics` / `.json` artifacts byte-for-byte.

## License

GPL-3.0-or-later.

[`icalendar`]: https://docs.rs/icalendar
