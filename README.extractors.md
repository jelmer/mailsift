# Writing extractors

An extractor is a small, standalone program that turns one incoming
RFC822 email into zero or more structured artifacts: a calendar event,
a receipt, a parcel-tracking record, a bill, a ticket. mailsift ships no
extractors of its own; it discovers yours at startup by scanning the
configured `extractors_dir` for `*.yaml` manifests. A ready-made
collection lives at
[mailsift-extractors](https://github.com/jelmer/mailsift-extractors).

The contract is deliberately small: mailsift feeds your program the raw
message on stdin and reads structured files back out of its working
directory. Everything else - which language you write in, how you parse
the mail, what libraries you pull in - is up to you.

## The two files

Each extractor is a pair living side by side in `extractors_dir`:

- `<name>.yaml` - the manifest: identity, dispatch hints, and a pointer
  to the script.
- `<name>.py` (or whatever the manifest names) - the executable script.

Files whose names begin with `.` or `_` are skipped during discovery, so
a `_lib/` directory of shared helpers or a `_tests/` directory of
fixtures stays invisible to the loader.

## Manifest YAML format

A manifest is named `<name>.yaml`. By default it pairs with a sibling
executable at `<name>.py`; the `script:` field overrides that when the
script lives elsewhere or under a different name.

```yaml
name: my-vendor              # required: unique identifier
order: 50                    # optional: lower runs earlier; default 100
script: my-vendor.py         # optional: defaults to <name>.py next to the manifest
from_domains:                # optional: dispatch hint, case-insensitive
  - vendor.example
  - "*.vendor.example"       # wildcard matches the bare domain and any subdomain
subject_regex: "(?i)..."     # optional: dispatch hint
requires:                    # optional: body-shape requirements (all must hold)
  - html                     #   message has a text/html part
  - text                     #   message has a text/plain part
  - "attachment:text/calendar"            # has a part of this MIME type
  - "attachment:filename:*.ics"           # has a part whose filename matches
require_dkim:                # optional: only run on a passing DKIM signature
  - vendor.example           #   from one of these domains (leading `.` = suffix
  - ".vendor.example"        #   match, subdomains only, not the bare domain)
```

### Fields

| Field           | Type            | Required | Notes |
|-----------------|-----------------|----------|-------|
| `name`          | string          | yes      | Unique across all discovered manifests. Used in logs and dedup bookkeeping. |
| `order`         | int             | no       | Default `100`. Lower numbers run earlier; ties break on manifest filename. |
| `script`        | string          | no       | Path to the executable, relative to the manifest's directory. Defaults to `<name>.py`. Must be `chmod +x`. |
| `from_domains`  | list of strings | no       | Case-insensitive match against the lowercased `From:` domain. `*.example.com` matches `example.com` and any subdomain. An empty list means "no `From:` constraint". |
| `subject_regex` | string          | no       | A regex applied to the `Subject:` header. Use `(?i)` for case-insensitive matching. |
| `requires`      | list of strings | no       | Body-shape requirements; every entry must be satisfied. Supported shapes: `html`, `text`, `attachment:<type>/<subtype>`, `attachment:filename:<pattern>`. Filename patterns accept a single leading or trailing `*` (e.g. `*.ics`, `boarding-*`). |
| `require_dkim`  | list of strings | no       | DKIM signing domains that authorise the message. Plain entries are exact matches; entries with a leading `.` are suffix matches against the signing domain (`.myshopify.com` matches `shop42.myshopify.com` but not `myshopify.com`). |

### Dispatch semantics

The manifest hints are cheap prefilters: they let mailsift decide, from
the headers and the MIME shape alone, whether to bother spawning your
script. Within one category, *at least one* entry must match (except
`requires`, where *every* entry must hold). Across categories, *all*
declared categories must match. An omitted or empty category means "no
constraint".

`from_domains`, `subject_regex` and `requires` are evaluated against the
parsed headers and (for `requires`) the message's MIME structure, so
whole classes of message can be skipped before your process ever starts.
`require_dkim` consults the topmost `Authentication-Results:` header;
messages that lack the header entirely are skipped. The milter
front-end, which sees mail before the MTA has authenticated it, can't
enforce `require_dkim` and opts out of that check at run time - use
`replay` or `imap-scan` for retroactive runs that need DKIM enforcement.

Getting the hints right matters for performance, but they never change
correctness: if a message slips through the prefilter, your script still
gets the final say by writing (or not writing) artifacts.

Validate your manifests before relying on them:

```sh
mailsift check --extractors /path/to/extractors   # bails on the first problem
mailsift lint  --extractors /path/to/extractors   # reports every problem at once
```

Both parse every YAML, compile each `subject_regex`, check the
`requires:` shapes, and confirm each named script exists and is
executable. `lint` additionally flags cross-directory name collisions,
which is handy as a pre-commit check.

## What an extractor must do

mailsift runs your extractor as a subprocess. The full input/output
contract is:

- **stdin**: the raw RFC822 message, unmodified.
- **cwd**: a fresh, empty per-run tempdir. Write whatever artifact files
  you like into it; mailsift reads them back when your process exits.
- **stdout / stderr**: captured to logs. **Not used** for artifact
  discovery - write files, not JSON to stdout.
- **exit code**: `0` for success (an empty cwd is fine and means
  "nothing to extract"); non-zero means "this extractor failed", which
  is logged and does not affect any other extractor or block mail
  delivery.
- **timeout**: per-extractor, default 10s. Your process is killed if it
  runs longer.

So the whole job is: read the message on stdin, decide what (if
anything) it contains, and drop named files into the current directory.

A Python extractor might open like:

```python
#!/usr/bin/env python3
import sys
from email import message_from_binary_file
from pathlib import Path

msg = message_from_binary_file(sys.stdin.buffer)

# ... inspect msg headers and parts, build your artifact ...

Path("flight-fr1234.event.ics").write_bytes(ics_bytes)
```

Nothing about the contract is Python-specific. A shell script, a Go
binary, a Rust program - anything executable that reads stdin and writes
files works equally well. The mailsift-extractors repo ships a small
Python helper (`_lib/mailsift_extractor.py`) that parses the message and
exposes the from-address, subject, decoded bodies, `application/ld+json`
blocks, and attachments; it's a convenience, not a requirement.

### Artifact filenames

mailsift classifies every file in cwd by its suffix. The part before the
suffix - the `<slug>` - is yours to choose and becomes the default
filename when the artifact is filed on disk.

Structure the slug as `<what-it-is>-<date>`: `ryanair-fr1234-2026-04-10`,
not `boarding-pass`. Kinds that carry their own identifying fields
(bills, receipts, reservations, ...) get renamed from those when filed,
so the slug is mostly a fallback there. Tickets are the exception - a
PDF or a pkpass has nothing readable inside it, so the slug is the only
name the blob will ever have, and every ticket called `boarding-pass`
lands on the same path. Use an ISO `YYYY-MM-DD` date so names sort
chronologically.

| Filename                   | Kind           | Required content |
|----------------------------|----------------|------------------|
| `<slug>.event.ics`         | `event`        | A valid iCalendar file. The `UID` inside is the dedup key. Multiple `VEVENT`s in one file are split into separate events. |
| `<slug>.reservation.json`  | `reservation`  | A schema.org-style reservation object (`FlightReservation`, `TrainReservation`, `BusReservation`, `LodgingReservation`, `EventReservation`, `FoodEstablishmentReservation`). mailsift converts it into a calendar event. |
| `<slug>.parcel.json`       | `parcel`       | Loose schema.org `ParcelDelivery` JSON. Must include `trackingNumber` (the dedup key). Merged with any prior record for the same tracking number as the parcel progresses. |
| `<slug>.receipt.json`      | `receipt`      | Loose schema.org `Order` / `Invoice` JSON. Must include `orderNumber` (or `identifier`) and a merchant/seller name. |
| `<slug>.bill.json`         | `bill`         | JSON with `payee`, `amount`, `dueDate`, `invoiceNumber`. |
| `<slug>.subscription.json` | `subscription` | schema.org-ish JSON carrying at least `subscriptionDuration`. Downstream tooling synthesises renewal reminders from it. |
| `<slug>.ticket.<ext>`      | `ticket`       | Any binary blob (PDF, pkpass, image, ...). Dedup is by content hash; `<ext>` is taken literally as the on-disk extension. The slug is the filed name, so make it specific: `ryanair-fr1234-2026-04-10.ticket.pdf`. |

Dotfiles and other `_*` files are skipped silently; any other
unrecognised filename in cwd is logged with a warning. Emit as many
artifacts as the message warrants - one boarding-pass email can produce
a `.ticket.pdf` and a `.reservation.json` at once.

A filed ticket gets a `<slug>.meta.json` sidecar beside it recording
the blob's filename and content type, plus the booking reference and
passenger from a `.reservation.json` emitted in the same run. Emitting
both from one message is what makes a ticket traceable back to its
trip.

Bills, parcels and subscriptions are **not** auto-synthesised into
calendar events. If you want a bill's due date to show on the calendar,
emit both a `.bill.json` *and* a `.event.ics` from the same run -
explicit beats clever.

### Recommended optional fields

The core fields above are the minimum. Everything else is passed through
untouched, but a handful of extra keys light up the downstream tooling
(web dashboard, calendar sinks, tracker registration) without any
kind-specific plumbing:

- `url` (any kind): a link to view or manage the artifact on the
  vendor's site. The web dashboard renders it as an "open" link next
  to the raw JSON. Kind-specific aliases are also honoured:
  `trackingUrl` for parcels, `orderUrl` for receipts, `paymentUrl` /
  `invoice.url` for bills, `managementUrl` for subscriptions.
  Non-`http(s)` URLs are silently dropped.
- For parcels: `trackingUrl` is the carrier's public tracking page.
  Extractors that can extract or synthesise one directly should; if
  they don't, mailsift synthesises one from `provider.@id` +
  `trackingNumber` for the well-known carriers (Royal Mail, DPD,
  Evri, and a few others).

### Dedup is mailsift's job, not yours

Extractors don't deal with duplicate detection, the on-disk layout, or
CalDAV. You just need to produce stable identifiers *inside* the
artifacts - the `UID` in an `.ics`, `trackingNumber` in a parcel,
`orderNumber` in a receipt, `invoiceNumber` in a bill - and mailsift
derives the dedup key from there. The same message replayed twice, or a
follow-up status email for the same parcel, collapses onto the same
record.

### Optional `_manifest.json`

An extractor may drop a `_manifest.json` in cwd with `notes` and
per-file `annotations`. It is purely informational: mailsift still
discovers artifacts by scanning cwd, and the manifest can neither add
nor remove them.

```jsonc
{
  "notes": ["matched ld+json FlightReservation for FR1234"],
  "annotations": {
    "flight-fr1234.reservation.json": { "confidence": "high", "source": "ld+json" }
  }
}
```

## Testing and debugging

Because the contract is "stdin in, files out", running an extractor by
hand is one line:

```sh
mkdir /tmp/run && cd /tmp/run && /path/to/extractors/my-vendor.py < saved-message.eml
ls
```

To exercise the full pipeline - dispatch, dedup, and filing - replay a
saved message through mailsift itself:

```sh
mailsift replay saved-message.eml --extractors /path/to/extractors --dry-run
```

Add `--explain` for a per-extractor dispatch table showing which
extractors matched, which were prefiltered out and why, and what each
producing extractor emitted - the quickest way to work out why your new
extractor did or didn't fire on a given message.

## Hello world: adding a new extractor

The rest of this document is the reference. This section is the
walkthrough - roughly what you'd do to go from "I have a forwarded
booking email" to a merged extractor in about twenty minutes. It
assumes you have a clone of
[mailsift-extractors](https://github.com/jelmer/mailsift-extractors)
checked out and `pytest` on your `PATH`.

Throughout, substitute `myvendor` with the vendor's slug. Keep it
lowercase, hyphen-separated, and roughly matching how the vendor
brands itself (`booking-com`, `air-france`, `sncf-connect`).

### 1. Save a corpus fixture and scrub the PII

Every extractor needs at least one real message to test against.
Save one from your mail client as `myvendor-<what-it-is>.eml` (for
example `myvendor-confirmation.eml`, `myvendor-cancellation.eml`)
under `tests/corpus/`. Use "Show original" in Gmail, "View source"
in Thunderbird, or `.eml` export from most desktop clients - you
want the raw RFC822 with headers intact, not the rendered body.

The corpus is public. Before committing, scrub anything personal
from both the headers and every MIME part (HTML body, text body,
attachments):

- Booking references, order numbers, tracking numbers, ticket
  tokens, receipt IDs: replace with a same-length placeholder like
  `XXXXXX` or `TESTID42` so length-based regexes still match.
- Personal names anywhere in the message (headers, body,
  attachment metadata): rewrite to `Joe Bloggs` or similar.
- Street addresses: strip to just a city or a plausible fake
  (`1 Example Street, London EX1 1EX`). Keep the country if the
  extractor keys off it.
- Email addresses in `To:`, `Cc:`, greetings, footers: rewrite to
  `you@example.com` or drop entirely if the extractor doesn't
  read them.
- Phone numbers, loyalty numbers, seat numbers if they identify
  you: replace with a value of the same shape.
- Dates and times: usually keep them - a lot of extractors do
  weekday/date arithmetic (see `dice.py` for an example) and
  shifting the date can break the resolution. If you must shift,
  shift the `Date:` header and the body dates by the same offset
  and re-check the day of week.

Keep the DKIM signature and `Authentication-Results:` header
verbatim even after scrubbing the body. `require_dkim` reads them
at replay time and a broken signature is fine (the pipeline doesn't
re-verify), but the header shape needs to be recognisable.

If in doubt, `grep` the scrubbed file for your own name, postcode,
and any real reference numbers before committing.

### 2. Copy the smallest representative extractor

Rather than starting from a blank file, copy an existing pair that
matches the shape of what you're building. Two good starting points
for single-artifact extractors:

- `dice.py` / `dice.yaml` - emits one `.reservation.json` from an
  HTML-only mail with no schema.org markup. Good template for
  event tickets and any parse-the-body-yourself vendor.
- `airbnb.py` / `airbnb.yaml` - emits one `.receipt.json` from an
  HTML body. Good template for payment receipts and orders.

For attachment-driven extractors (the vendor mails a PDF and the
body is boilerplate), `hyperoptic.py` is the smallest example.

Copy both files:

```sh
cp extractors/dice.py    extractors/myvendor.py
cp extractors/dice.yaml  extractors/myvendor.yaml
chmod +x extractors/myvendor.py
```

The script needs to be executable; mailsift `exec`s it directly.

### 3. Adjust the manifest

Open `extractors/myvendor.yaml`. Every field is documented in the
reference above; the settings that matter for a new extractor:

```yaml
name: myvendor
order: 50
from_domains:
  - myvendor.example
subject_regex: "(?i)^Your booking confirmation"
requires:
  - html
require_dkim:
  - myvendor.example
```

- `name` must be unique across the whole `extractors_dir`.
  Convention is to match the filename stem.
- `from_domains` is the first-line prefilter. List every domain
  the vendor sends from. If they use subdomains
  (`no-reply.myvendor.example`), add `"*.myvendor.example"` too.
  Case is ignored.
- `subject_regex` is the second prefilter. Be specific enough to
  exclude marketing mail (`^Your booking confirmation` beats
  `booking`). Anchor with `^` when the vendor has a stable
  subject prefix. Always use `(?i)` for case-insensitivity unless
  the vendor is fussy about case.
- `requires` gates on the MIME shape. `html` for HTML-body
  extractors, `text` for text-only, `attachment:application/pdf`
  or `attachment:filename:*.pdf` if you need a PDF attachment.
  Every entry must match, so keep the list minimal.
- `require_dkim` is the strongest guarantee that the message is
  really from the vendor. Set it to the domain the vendor signs
  from - usually the same as `from_domains`, but some vendors
  sign from a subdomain or a delivery partner. Leave it out only
  if the vendor genuinely doesn't sign (rare in 2026), and
  document why in a comment. The milter path skips this check;
  `replay` and `imap-scan` enforce it.

`order: 50` runs earlier than the default `100`; use it when your
extractor should take precedence over the generic `schema-ld`
fallback. Otherwise leave it at the default.

Validate your changes:

```sh
mailsift lint --extractors extractors
```

`lint` compiles every regex, walks the `requires:` shapes, checks
the script exists and is executable, and reports every problem at
once. Fix anything it complains about before moving on.

### 4. Adjust the Python script

Open `extractors/myvendor.py`. The wire contract is the same one
described earlier: stdin is the raw message, cwd is a fresh
tempdir, output is `<slug>.<kind>.<ext>` files in cwd. The
`_lib/mailsift_extractor.py` helper hands you a parsed `Mail` with
`.from_address`, `.subject`, `.date`, `.text`, `.html`,
`.ld_json`, and `.attachments`; use it or parse the message
yourself, whichever fits.

Decide up front which artifact kind you're producing. The table
under "Artifact filenames" above is the source of truth: pick the
suffix that matches your message and populate the required fields
listed there. In particular:

- `.reservation.json` needs a schema.org reservation type
  (`FlightReservation`, `TrainReservation`, `LodgingReservation`,
  `EventReservation`, `FoodEstablishmentReservation`, ...) and
  ideally a `reservationNumber` so dedup works.
- `.receipt.json` needs at least a merchant name and an
  `orderNumber` (or `identifier`).
- `.parcel.json` needs a `trackingNumber`.
- `.bill.json` needs `payee`, `amount`, `dueDate`, `invoiceNumber`.
- `.ticket.<ext>` is an opaque blob; pair it with a
  `.reservation.json` in the same run so the meta sidecar can
  link the two.

Pick a stable slug. `dice.py` uses `dice-<order_id>`; use
something similar so the tempdir filenames are readable in logs.
The slug is what shows up in the meta sidecar for tickets.

Exit 0 when you're done, even if you wrote nothing - an empty cwd
just means "not for me". Only exit non-zero on real, unexpected
failures; those get logged.

### 5. Add a pytest test

The test harness in `extractors/_tests/conftest.py` runs your
extractor in a tempdir and returns a `{filename: parsed_body}`
dict. Test files follow the pattern `test_<vendor with hyphens
replaced by underscores>.py` (so `myvendor` stays `test_myvendor.py`,
but `air-france` becomes `test_air_france.py`).

```python
"""Tests for the MyVendor booking extractor."""

from __future__ import annotations


def test_confirmation_emits_reservation(run_extractor):
    out = run_extractor("myvendor", "myvendor-confirmation.eml")
    assert set(out) == {"myvendor-XXXXXX.reservation.json"}
    reservation = out["myvendor-XXXXXX.reservation.json"]
    assert reservation["@type"] == "EventReservation"
    assert reservation["reservationNumber"] == "myvendor-XXXXXX"
```

The first argument to `run_extractor` is the extractor name
(matches `<name>.py` under `extractors/`); the second is the eml
filename under `tests/corpus/`. JSON artifacts come back as parsed
dicts. iCalendar files come back as strings with the volatile
`DTSTAMP` line stripped so the body compares stably. Binary blobs
(tickets, receipt-file sidecars) come back as raw `bytes`.

Assert on the full artifact where you can (`assert reservation ==
{...}`) rather than picking off individual keys; a full-body
assertion catches regressions in fields you didn't think to check.

### 6. Run the tests

```sh
pytest extractors/_tests/test_myvendor.py -v
```

Iterate on the extractor until it passes. Then run the whole
suite so you're sure you haven't disturbed anyone else's
dispatch:

```sh
pytest extractors/_tests
```

A test that fails on JSON keys usually means either your slug
changed (the `set(out)` assertion catches this) or a field
you're extracting has moved in the source HTML - re-check the
scrubbed fixture and adjust the parser.

### 7. Verify against the pipeline end to end

The pytest harness runs your extractor in isolation. To confirm
that mailsift itself dispatches to it - that the manifest hints
match and no earlier extractor claims the message first - replay
the fixture through mailsift with a dry run:

```sh
mailsift replay tests/corpus/myvendor-confirmation.eml \
    --extractors extractors --dry-run --explain
```

The `--explain` table lists every extractor, whether it matched
or was prefiltered out and why, and what each producing extractor
emitted. If `myvendor` isn't in the "matched" column, the
manifest hints are wrong: check the `From:` domain, the DKIM
domain, and the subject regex against the fixture. `--dry-run`
prevents the run from actually filing anything to your local
event / bill / parcel directories.

See the "Testing and debugging" section above for the
single-message invocation without the pipeline, which is useful
when you want to see the raw files your extractor drops in cwd.

### 8. Commit checklist

Before opening the PR:

- `ruff format extractors/myvendor.py extractors/_tests/test_myvendor.py`
- `ruff check extractors/myvendor.py extractors/_tests/test_myvendor.py`
- `pytest extractors/_tests` passes
- `mailsift lint --extractors extractors` is clean
- `mailsift replay tests/corpus/myvendor-*.eml --extractors extractors --dry-run --explain`
  shows your extractor matching and emitting the expected artifact
- The corpus fixtures under `tests/corpus/` have been scrubbed of
  personal data (names, addresses, real booking references,
  personal email addresses)
- The artifact filenames follow the `<slug>.<kind>.<ext>` scheme
  from the "Artifact filenames" table

That's the whole loop. Once the first extractor is in, adding
neighbours (the vendor's cancellation mail, refund mail, itinerary
update) is a matter of a new corpus fixture and a new branch in the
existing script.
