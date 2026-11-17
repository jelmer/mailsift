# Mailsift Gmail Add-on

A Google Workspace Add-on for Gmail that shows, next to an open message, what Mailsift extracted from it (for example a train's route, departure and booking reference, or a parcel's tracking number), with links to those records in Mailsift.

The add-on doesn't send mail to Mailsift. Gmail add-ons can't act on incoming mail, so that is left to `mailsift imap-scan --watch` or the milter. They record what each message produced in `seen.db`, and the add-on looks the open message up by its `Message-ID` with `GET /api/messages?message_id=...`. Mail processed before that recording existed has to be scanned again to show up.

## Server Setup

Run `mailsift web` (built with `--features web`) as the same user as the scanner or milter, so it reads the same `seen.db`:

```sh
cargo install --path .. --features web
mailsift web --listen 127.0.0.1:8088
```

If the server sits behind an authenticating reverse proxy, set an `Authorization` header (`Bearer <token>` or `Basic <credentials>`) in the add-on's Settings.

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
