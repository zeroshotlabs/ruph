# ruph Quick Start

`ruph` serves websites from a folder on disk.

In most cases, you point it at your site folder and start it:

```bash
./ruph /var/www/mysite
```

If you want HTTPS:

```bash
./ruph /var/www/mysite --tls
```

If you use a config file:

```bash
./ruph -c ruph.ini /var/www/mysite
```

## Basic Idea

Your site lives in a folder called the `docroot`.

Inside that folder, ruph can do two things:

1. Serve normal files directly
2. Run `_index.php` files to customize behavior

That means you can start simple with plain HTML files, and only add PHP control files where you need them.

## Simplest Site

Put an `index.html` in your site folder:

```text
/var/www/mysite/
  index.html
```

Requests for `/` will serve that file directly.

## Site-Wide Rules

If you want one PHP file to control your whole site, add `_index.php` at the top of the site:

```text
/var/www/mysite/
  _index.php
  index.html
  about.html
```

Use this for things like:

- redirects
- access rules
- custom 404 pages
- logging

If `_index.php` does nothing and stays silent, ruph continues and serves the normal file.

If `_index.php` sends output, changes status, redirects, or exits, that becomes the response.

## Folder-Specific Rules

You can also put `_index.php` inside a subfolder:

```text
/var/www/mysite/
  _index.php
  blog/
    _index.php
    post1.html
```

This lets one folder behave differently from the rest of the site.

Example uses:

- add a banner to archived pages
- redirect old URLs in one section
- add special headers for files in one folder

If a real file exists in that folder, ruph normally serves it directly.

But if that folder has its own `_index.php`, that file can intercept the request and:

- handle the response itself
- or let the normal file be served

## Simple Rule of Thumb

- normal files are fast and are served directly
- `_index.php` lets you step in when you need custom behavior
- put `_index.php` only where you want control

## Existing PHP Files

If the requested file itself ends in `.php`, ruph executes that file as PHP.

Example:

```text
/var/www/mysite/
  contact.php
```

Requesting `/contact.php` runs that file.

## Virtual Hosts

If you host more than one site, use `ruph.ini` to point each domain to its own folder.

Example:

```ini
[https.example.com]
docroot = /var/www/example.com

[https.other.org]
docroot = /var/www/other.org
```

Each site can then have its own files and its own `_index.php`.

## When to Use `_index.php`

Use `_index.php` when you want to:

- redirect URLs
- protect part of a site
- show a custom error page
- modify a static file before sending it

Do not use `_index.php` if you only need to serve normal files.

## Good First Step

Start with this:

```text
/var/www/mysite/
  index.html
```

Then add `_index.php` later only if you need custom behavior.

## More Detail

For the full request flow and exact `_index.php` behavior, see [REQUESTS.md](REQUESTS.md).

For config file details, see [RUPH_INI.md](RUPH_INI.md).
