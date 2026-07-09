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

#### Gmail

Gmail rejects your normal password over IMAP, so you have two ways in.

**App password (simplest).** If the account has 2-Step Verification on,
create an [app password](https://myaccount.google.com/apppasswords),
drop it in a file, and use it like any other IMAP password:

```sh
mkdir -p ~/.config/mailsift
(umask 077; cat > ~/.config/mailsift/gmail.pass)   # paste the app password, then Ctrl-D
mailsift imap-scan imaps://you@imap.gmail.com/INBOX \
    --password-file ~/.config/mailsift/gmail.pass --since 01-Jan-2026
```

Reading the password from a `cat` prompt keeps it out of your shell
history. Spaces in the pasted app password are fine; `--password-file`
uses the file verbatim after trimming surrounding whitespace. Workspace
admins can disable app passwords, in which case use OAuth2 below.

**OAuth2 (XOAUTH2), fixed token.** Pass a short-lived bearer token via
`--oauth2-token-file` instead of `--password-file`. The file holds just
the access token (surrounding whitespace is trimmed):

```sh
oauth2l fetch --type=bearer \
    --scope=https://mail.google.com/ \
    --output_format=bare > ~/.cache/mailsift/gmail.token
mailsift imap-scan imaps://you@imap.gmail.com/INBOX \
    --oauth2-token-file ~/.cache/mailsift/gmail.token --since 01-Jan-2026
```

[`oauth2l`](https://github.com/google/oauth2l) runs the browser consent
flow the first time and caches a refresh token, so later `fetch` calls
are non-interactive. Workspace accounts can instead use a service
account with domain-wide delegation. The token file is read once at
startup and Gmail access tokens expire after ~1 hour, so this mode only
fits a one-off scan that finishes within the token's lifetime. For a
long `--watch` session, use the refresh-token mode below.

**OAuth2 (XOAUTH2), refresh token.** Give mailsift a long-lived refresh
token plus your app's client id (and, for Google, its client secret) and
it mints a fresh access token at every connect, so it survives token
expiry across reconnects and `--watch` sessions. The provider's token
endpoint is derived from the IMAP host for Gmail and Outlook; override
it with `--oauth2-provider google|microsoft` or a full
`--oauth2-token-endpoint` for anything else.

```sh
mailsift imap-scan imaps://you@imap.gmail.com/INBOX \
    --oauth2-refresh-token-file ~/.config/mailsift/gmail.refresh \
    --oauth2-client-id "$CLIENT_ID.apps.googleusercontent.com" \
    --oauth2-client-secret-file ~/.config/mailsift/gmail.secret \
    --watch
```

Obtaining the refresh token itself still needs a one-time browser
consent; `oauth2l`, the provider's OAuth playground, or any OAuth2
client library can produce one. Google desktop-app clients carry a
client secret; Microsoft public (native) clients omit
`--oauth2-client-secret-file`.

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

### Dovecot Sieve

There is no dedicated Sieve mode: `replay -` already fits the Sieve
pipe contract (raw RFC822 on stdin, run the pipeline, exit 0). Sieve
runs during local delivery, after Dovecot has added its
`Authentication-Results:` header, so unlike the milter this path *does*
enforce `require_dkim`.

Enable the `sieve_extprograms` plugin and the `pipe` extension:

```
# dovecot / pigeonhole plugin block
plugin {
  sieve_plugins       = sieve_extprograms
  sieve_extensions    = +vnd.dovecot.pipe
  sieve_pipe_bin_dir  = /usr/lib/dovecot/sieve-pipe
}
```

Programs in `sieve_pipe_bin_dir` are invoked with a fixed argv, so drop
a wrapper there rather than symlinking the binary directly:

```sh
# /usr/lib/dovecot/sieve-pipe/mailsift
#!/bin/sh
exec /usr/local/bin/mailsift --config /etc/mailsift/config.toml replay -
```

Then pipe delivered mail through it:

```sieve
require ["vnd.dovecot.pipe"];
pipe :copy "mailsift";
```

The `:copy` modifier is load-bearing. Without it `pipe` counts as the
message's delivery action and the mail never reaches the mailbox; with
it mailsift gets a copy and normal delivery proceeds untouched. Put the
rule in a `sieve_before` script to run it on every delivery ahead of
users' own filters.

## Extractors

A collection of ready-to-use extractors lives at
[mailsift-extractors](https://github.com/jelmer/mailsift-extractors).

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

For the full extractor contract - manifest fields, dispatch semantics,
artifact filenames, and how to test one - see
[README.extractors.md](README.extractors.md).

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
