# chisel.rs

chisel is a static site generator, and a port of [its python sibling][ck]. 

Under `$HOME/Sites`, chisel expects the following folders:

- `bare` with [minijinja] compliant templates
- `notes` with markdown files (with extension, say, .md)

A typical note format is as follows:

```md
Title
2021-04-15 21:52

This is now the body of the post. By default, the body is evaluated and parsed with markdown.

Another line.
```

Markdown parser is comrak, and supports markdown extension features, viz., footnotes, fenced code, smartypants, and tables.

All raw HTML is omitted. For instance `<figure>` block will be removed and so on. Any HTML block like details may also be omitted. Hence avoid using raw HTML blocks within notes.

[ck]: https://github.com/ckunte/chisel
[minijinja]: https://docs.rs/minijinja/latest/minijinja/

