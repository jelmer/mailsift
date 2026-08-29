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
| `.reservation.json`| schema.org reservation (Flight/Train/Bus/Lodging/Event/FoodEstablishment). Converted to a single VEVENT, and archived as JSON when `reservations_dir` is set. |
| `.bill.json`       | Loosely schema.org `Invoice`-shaped record (payee, invoice number, due date, ...).  |
| `.parcel.json`     | schema.org `ParcelDelivery`-shaped record (merged across status-update mails).      |
| `.receipt.json`    | Loosely schema.org `Order`-shaped record (merchant, order number, date, ...).       |
| `.ticket.<ext>`    | Opaque ticket / boarding pass blob (`pdf`, `pkpass`, image formats). Filed with a `.meta.json` sidecar describing it. |

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
reservations_dir = "/home/jelmer/Documents/reservations"

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

**OAuth2 (XOAUTH2), recommended.** Run `mailsift imap-auth` once to do
the browser consent flow and write a JSON credential bundle, then point
`imap-scan` at it. mailsift mints a fresh access token at every connect,
so this survives token expiry across reconnects and `--watch` sessions.

```sh
mailsift imap-auth you@gmail.com \
    --client-id "$CLIENT_ID.apps.googleusercontent.com" \
    --client-secret-file ~/.config/mailsift/gmail.client-secret \
    --output ~/.config/mailsift/gmail.json
mailsift imap-scan imaps://you@imap.gmail.com/INBOX \
    --oauth2-credentials-file ~/.config/mailsift/gmail.json --watch
```

`imap-auth` starts a temporary server on `127.0.0.1`, opens your browser
at the provider's consent screen, and captures the result; pass
`--no-browser` to print the URL and paste the redirect back instead (for
headless / SSH sessions). The provider is derived from the account
domain, or name it with `--provider google|microsoft`. The client id and
secret come from an OAuth2 client you register with the provider (a
"Desktop app" client for Google; a public/native client for Microsoft,
which has no secret so you omit `--client-secret-file`). The bundle is
written owner-readable and holds a long-lived refresh token, so keep it
somewhere private.

The bundle can also be assembled by hand (or from an existing refresh
token) with the discrete flags: `--oauth2-refresh-token-file`,
`--oauth2-client-id`, `--oauth2-client-secret-file`, and either an
IMAP-host-derived provider or an explicit `--oauth2-provider` /
`--oauth2-token-endpoint`.

**OAuth2 (XOAUTH2), fixed token.** For a one-off scan that finishes
within an hour, pass a short-lived bearer token via `--oauth2-token-file`
instead. The file holds just the access token (whitespace trimmed):

```sh
oauth2l fetch --type=bearer \
    --scope=https://mail.google.com/ \
    --output_format=bare > ~/.cache/mailsift/gmail.token
mailsift imap-scan imaps://you@imap.gmail.com/INBOX \
    --oauth2-token-file ~/.cache/mailsift/gmail.token --since 01-Jan-2026
```

Gmail access tokens expire after ~1 hour and the file is read once at
startup, so a long `--watch` session outlives it; use the credential
bundle above for that.

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

### `maildir-scan`: walk a Maildir on disk

```sh
mailsift maildir-scan /srv/mail/jelmer/Maildir
mailsift maildir-scan /srv/mail/jelmer/Maildir --recurse
mailsift maildir-scan /srv/mail/jelmer/Maildir --recurse --since 2026-01-01
```

Reads `cur/` and `new/` (`tmp/` is skipped) and runs each message
through the pipeline. With `--recurse`, also descends into Maildir++
subfolders (`.name/cur`, `.name/new`); non-Maildir dotdirs are skipped.
Useful for one-off backfills against archived mail without going through
an IMAP server. Like `imap-scan`, this mode bypasses the milter's dedup
store and stats recorder; upstream sinks (CalDAV etc.) are idempotent.

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

### `web`: browse extracted artifacts

Build with the optional `web` feature:

```sh
cargo install --path . --features web
```

Then serve a read-only HTML dashboard over the configured
`bills_dir` / `parcels_dir` / `receipts_dir` / `subscriptions_dir` /
`events_dir` / `tickets_dir`:

```sh
mailsift web --listen 127.0.0.1:8088          # TCP
mailsift web --listen unix:/run/mailsift.sock  # unix socket
```

The dashboard rescans the artifact directories on every request, so
it happily sits alongside a running milter or `imap-scan --watch`.
JSON views are exposed at `/api/bills.json`, `/api/parcels.json`,
`/api/receipts.json`, and `/api/subscriptions.json` for scripting.
Raw `.ics` and ticket blobs are served with their proper
Content-Type so a browser can open them directly.

No authentication is built in; bind to loopback (or put it behind a
reverse proxy) if the artifacts are personal.

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
