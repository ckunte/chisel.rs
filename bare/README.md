# Basic templates

A minimal [minijinja] template set to get a chisel site running: a home
page, a single-post page with prev/next links, an archive listing, and a
JSON feed. Use these as a starting point and adjust to taste.

## Setup

chisel expects templates in `~/Sites/templates` (see the top-level README).

```
cp templates/*.j2 ~/Sites/templates/
cp templates/style.css ~/Sites/home.lo/style.css
```

Then edit `sitesettings.j2` with your own site name, author, url, and
description.

## Files

- `sitesettings.j2` -- site-wide variables (title, author, url, description)
- `base.j2` -- shared HTML layout (head, header, footer); other templates extend this
- `home.j2` -- home page (`index.html`); includes `archive.j2`, links to `feed.json`
- `detail.j2` -- single note page; includes `nav.j2` for prev/next links
- `archive.j2` -- full, newest-first list of notes; included by `home.j2`
- `nav.j2` -- prev/next footer links for `detail.j2`
- `feed.j2` -- JSON Feed 1.1 (`feed.json`), all notes
- `style.css` -- plain, dependency-free stylesheet (light/dark aware)
