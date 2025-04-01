# mailsift

Extracts structured artifacts from incoming email.

For each message, mailsift runs a set of small extractor scripts. Each
extractor reads the raw RFC822 from stdin and writes typed artifact
files into a per-run tempdir:

| Suffix       | What it is                                                          |
|--------------|---------------------------------------------------------------------|
| `.event.ics` | iCalendar event (parsed and re-emitted via the `icalendar` crate).  |

Events go to a local `<UID>.ics` directory.

Extraction is best-effort: failed extractors log and the next message
continues.

## Install

```sh
cargo install --path .
```

## Usage

```sh
mailsift replay --extractors /path/to/extractors --events-dir /path/to/out message.eml
```

## Extractor contract

Each extractor is paired with a YAML manifest declaring its name and the
script to run. The script reads the raw RFC822 message on stdin and
writes typed artifact files into its current working directory; mailsift
hands it an empty tempdir and scans the directory after the run.

Output files use the pattern `<slug>.<kind>.<ext>`. Today the only
recognised kind is `event` (with `ics` as the extension).

Exit zero on success (an empty cwd is fine) and non-zero on failure.

## License

GPL-3.0-or-later.
