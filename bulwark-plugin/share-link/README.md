# Public links: a Bulwark plugin

This plugin adds **Get public link** to a file's menu in Bulwark's Files. A dialog then shows the file's links, with **Copy** and **Revoke** buttons, and makes new ones valid for 1 day, 7 days, 30 days or 1 year. Anyone with a link can download the file without signing in, until the link expires.

It needs a Stalwart server with share links, built from this fork (see `FORK.md` at the top of the repository).

## Installing

1. Zip the two files, with no folder inside the zip:

   ```bash
   cd bulwark-plugin/share-link && zip ../share-link.zip manifest.json index.js
   ```

2. In Bulwark's admin console, open **Plugins**, upload `share-link.zip` and enable it. The plugin needs no build step.

## The proxy in front of Bulwark

The plugin talks to the server with Bulwark's `api.http.post`. That call goes to **Bulwark's own address**, not to Stalwart's, and carries the user's Stalwart login. Two paths on the webmail's address therefore have to reach Stalwart:

| Path on the webmail's address | Goes to |
|---|---|
| `/api/sharelink/` | Stalwart, same path. The plugin's calls. |
| `/s/` | Stalwart, same path. The public links themselves. |

Keep the `Host` header and set `X-Forwarded-Proto`. Stalwart builds the link it hands out from them, so a link reads `https://<webmail address>/s/<token>`.

Caddy:

```
mail.example.com {
    @stalwart path /api/sharelink/* /s/*
    reverse_proxy @stalwart stalwart:8080
    reverse_proxy bulwark:3000
}
```

nginx:

```nginx
location /api/sharelink/ { proxy_pass http://stalwart:8080; proxy_set_header Host $host; proxy_set_header X-Forwarded-Proto $scheme; }
location /s/             { proxy_pass http://stalwart:8080; proxy_set_header Host $host; proxy_set_header X-Forwarded-Proto $scheme; proxy_buffering off; }
location /               { proxy_pass http://bulwark:3000; }
```

Two more cases:

- **Same address.** If Bulwark and Stalwart already share one address and Stalwart answers there, there is nothing to add.
- **Bulwark under a path.** If Bulwark is mounted under a path such as `/webmail`, the plugin's calls arrive as `/webmail/api/sharelink/...`. Strip the prefix when forwarding them to Stalwart.

## What it does and doesn't do

- **Files only.** Folders get a message instead of a dialog.
- **Your own files only.** Bulwark doesn't tell plugins which account a file is in, so the plugin sends the file's name and size along. The server refuses when they don't match, rather than linking another file with the same id. For a file someone shared with you, that means a "does not match" message.
- **Copy.** Copy uses the browser clipboard. Where the browser refuses, the link is selected so Ctrl+C copies it.
- **Permissions.** The plugin asks for `ui:files` (the menu entry), `files:read` (to know which file was clicked), `ui:observe` (its dialog) and `http:post` (only to `/api/sharelink/`).
