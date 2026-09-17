# mailsift

A tool that watches your email and pulls out the structured bits:
calendar events, bills, parcels, receipts, tickets, subscriptions.
Your inbox already contains most of the data you care about - flight
times, tracking numbers, invoice due dates, restaurant bookings - and
a small program can lift it out into proper files and feeds.

For each incoming message mailsift runs a set of small per-vendor
extractor scripts. Each reads the raw RFC822 on stdin and writes typed
artifact files into a per-run tempdir:

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
system GSSAPI library (MIT Kerberos or Heimdal). The `gssapi` feature
gates SASL `GSSAPI` for IMAP and HTTP `Negotiate` for CalDAV; both fall
back to basic auth. To build without Kerberos:

```sh
cargo install --path . --no-default-features
```

## Configure

mailsift reads `$XDG_CONFIG_HOME/mailsift/config.toml` (typically
`~/.config/mailsift/config.toml`); `--config <path>` overrides it. See
`config.example.toml` for the shape; every key is optional.

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
optional port, optional mailbox path; without a user the current OS user
is used. With the `gssapi` feature, omit `--password-file` to
authenticate via Kerberos. The mailbox is selected **read-only**.

#### Gmail

Gmail rejects normal passwords over IMAP, so use either an app password
or OAuth2.

**App password.** With 2-Step Verification on, create an [app
password](https://myaccount.google.com/apppasswords), put it in a file,
and use it like any other IMAP password:

```sh
mkdir -p ~/.config/mailsift
(umask 077; cat > ~/.config/mailsift/gmail.pass)   # paste, then Ctrl-D
mailsift imap-scan imaps://you@imap.gmail.com/INBOX \
    --password-file ~/.config/mailsift/gmail.pass --since 01-Jan-2026
```

Spaces in the pasted password are fine; the file is used verbatim after
trimming surrounding whitespace. Workspace admins can disable app
passwords, in which case use OAuth2.

**OAuth2 (XOAUTH2).** Run `mailsift imap-auth` once to do the browser
consent flow and write a JSON credential bundle, then point `imap-scan`
at it. A fresh access token is minted at every connect, so this survives
token expiry across reconnects and `--watch` sessions.

```sh
mailsift imap-auth you@gmail.com \
    --client-id "$CLIENT_ID.apps.googleusercontent.com" \
    --client-secret-file ~/.config/mailsift/gmail.client-secret \
    --output ~/.config/mailsift/gmail.json
mailsift imap-scan imaps://you@imap.gmail.com/INBOX \
    --oauth2-credentials-file ~/.config/mailsift/gmail.json --watch
```

`imap-auth` starts a temporary server on `127.0.0.1` and opens your
browser; `--no-browser` prints the URL and reads the redirect back
instead. The provider is derived from the account domain, or named with
`--provider google|microsoft`. The client id and secret come from an
OAuth2 client you register with the provider (a "Desktop app" client for
Google; a public/native client for Microsoft, which has no secret). The
bundle holds a long-lived refresh token and is written owner-readable.

A bundle can also be assembled by hand from an existing refresh token
with `--oauth2-refresh-token-file`, `--oauth2-client-id`,
`--oauth2-client-secret-file` and `--oauth2-provider` /
`--oauth2-token-endpoint`.

For a one-off scan finishing within the hour, `--oauth2-token-file`
takes a plain short-lived bearer token instead. It is read once at
startup, so it will not outlast a long `--watch` session.

A progress bar shows scan progress when stderr is a TTY; one summary
line per message names the UID, extractor, and what was extracted:

```
INFO event updated target=/home/jelmer/.../flight-ezy2521@mailsift.ics
INFO extracted from UID 1234: easyjet=2 events
```

Add `--watch` to stay connected after the initial scan and process new
messages as they arrive (IMAP IDLE, RFC 2177), reconnecting with
exponential backoff on transport errors. `--limit` then applies only to
the initial backfill.

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

Reads `cur/` and `new/` (`tmp/` is skipped); `--recurse` also descends
into Maildir++ subfolders. Useful for one-off backfills against archived
mail. Like `imap-scan`, it bypasses the milter's dedup store and stats
recorder; upstream sinks (CalDAV etc.) are idempotent.

### `milter`: Postfix milter

```sh
mailsift milter --socket unix:/run/mailsift/milter.sock
```

Runs the pipeline at end-of-message and always returns `Continue`, so
extraction failures never block delivery. A wall-clock deadline
(default 20 s) caps each message.

The milter sees mail before the local MTA's DKIM check has run, so it
can't enforce extractor-level `require_dkim` and skips that check. Use
`replay`/`imap-scan` for runs that do want DKIM enforcement.

### `web`: browse extracted artifacts

Build with the optional `web` feature:

```sh
cargo install --path . --features web
```

Then serve a read-only HTML dashboard over the configured
`bills_dir` / `parcels_dir` / `receipts_dir` / `subscriptions_dir` /
`events_dir` / `reservations_dir` / `tickets_dir`:

```sh
mailsift web --listen 127.0.0.1:8088          # TCP
mailsift web --listen unix:/run/mailsift.sock  # unix socket
```

The dashboard rescans the artifact directories on every request, so it
happily sits alongside a running milter or `imap-scan --watch`. JSON
views for scripting live at `/api/bills.json`, `/api/parcels.json`,
`/api/receipts.json`, `/api/subscriptions.json` and
`/api/reservations.json`; raw `.ics` and ticket blobs are served with
their proper Content-Type.

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

The `:copy` modifier is load-bearing: without it `pipe` counts as the
message's delivery action and the mail never reaches the mailbox. Put
the rule in a `sieve_before` script to run it ahead of users' own
filters.

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
the helper at `extractors/_lib/mailsift_extractor.py`. Exit 0 means
"done, look at my output"; non-zero means "I failed, skip me".

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
pipeline and compare the resulting artifacts byte-for-byte.

## License

GPL-3.0-or-later.

[`icalendar`]: https://docs.rs/icalendar
