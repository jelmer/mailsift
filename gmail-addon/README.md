# Mailsift Gmail Add-on

A Google Workspace Add-on for Gmail that fetches extractor manifests from a `mailsift web` server (`GET /api/extractors`), matches messages in Gmail against the manifest rules (`from_domains`, `subject_regex`, `requires`, `require_dkim`), and uploads matching raw RFC822 messages to `POST /api/messages` for extraction.

## Features

- **Contextual Sidebar Trigger**: When opening an email in Gmail, checks whether the message matches any extractor manifest from your Mailsift server, optionally auto-uploads matching messages, and shows a button to upload (or dry-run) any email manually.
- **schema-ld Prefilter**: The `schema-ld` extractor's manifest only requires an HTML body, so the add-on additionally requires an `application/ld+json` block in the HTML before treating a message as a match. Otherwise nearly every HTML email would be uploaded.
- **Background Sync**: Uploads matching raw RFC822 messages and applies the `Mailsift` label to their threads. The hourly time-driven trigger remembers how far it got and searches only mail received since then (`after:<time>`, at most 7 days back, narrowed to `from:(...)` when all extractors declare `from_domains`), working oldest first, so a run that runs out of time is continued by the next. Uploads the server rejects are retried by later runs until they are older than 7 days; failing runs throw, so Apps Script reports them. The manual sync from the add-on scans the newest 50 unlabelled threads from the last 7 days in short batches.
- **External Auth Support**: Supports an optional `Authorization` header (`Bearer <token>` or `Basic <credentials>`) in Settings for Mailsift servers behind an authenticating reverse proxy.

## Server Setup

Run `mailsift web` (built with `--features web`) with `extractors_dir` and your target artifact directories configured in `config.toml`:

```sh
cargo install --path .. --features web
mailsift web --listen 127.0.0.1:8088
```

The server exposes:
- `GET /api/extractors` — JSON array of discovered extractor manifests (`name`, `order`, `from_domains`, `subject_regex`, `requires`, `require_dkim`, `unsigned_before`). Responses carry an `ETag`; the add-on uses its cached copy for 5 minutes and then revalidates it with `If-None-Match`, so it never needs a manual refresh.
- `POST /api/messages` — Accepts a raw `message/rfc822` body (with optional `?source=...&received_at=...&dry_run=true` query parameters), runs the extraction pipeline, and returns a JSON summary of matched extractors and filed artifact counts.

## Installing into Gmail

### Option 1: Using `@google/clasp`

1. Install and log in to `clasp`:
   ```sh
   npm install -g @google/clasp
   clasp login
   ```
2. Create a new Apps Script project inside `gmail-addon/`:
   ```sh
   cd gmail-addon
   clasp create --type standalone --title "Mailsift"
   clasp push
   ```
3. Open the project in the Apps Script editor (`clasp open`), click **Deploy > Test deployments**, select **Google Workspace Add-on**, and click **Install** to install it into your Gmail account.

### Option 2: Google Workspace Marketplace Publishing

1. Open the Apps Script project settings and link it to a Google Cloud project.
2. In the Google Cloud Console, enable the **Google Workspace Marketplace SDK**.
3. Under **Google Workspace Marketplace SDK > App Configuration**, select **Gmail Add-on**, enter the Apps Script deployment ID, and publish either privately to your Google Workspace domain or publicly.

## Running Unit Tests

```sh
cd gmail-addon && npm test
```
