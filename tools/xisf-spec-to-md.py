#!/usr/bin/env python3
"""Convert the XISF 1.0 specification HTML into Markdown.

The XISF specification is published by Pleiades Astrophoto as HTML. Reading it
as Markdown makes it greppable and diffable, which matters when implementing a
decoder against it.

Dependency-free (stdlib html.parser only), so it runs anywhere python3 does.

Usage:
    python3 tools/xisf-spec-to-md.py INPUT.html [-o OUTPUT.md]

NOTE ON COPYRIGHT: the output is a format-shifted copy of Pleiades Astrophoto's
documentation. Keep it OUT of version control (see .gitignore). This script is
ours and is committed; the converted specification is not redistributed.
"""

import argparse
import html
import re
import sys
from html.parser import HTMLParser

# Tags whose entire subtree is dropped.
SKIP_TREE = {"script", "style", "head", "noscript"}

BLOCK = {
    "p", "div", "section", "article", "header", "footer", "blockquote",
    "h1", "h2", "h3", "h4", "h5", "h6", "ul", "ol", "dl", "table", "tr", "pre",
}


class SpecToMarkdown(HTMLParser):
    def __init__(self):
        super().__init__(convert_charrefs=True)
        self.out = []          # emitted markdown chunks
        self.skip_depth = 0    # >0 while inside a SKIP_TREE subtree
        self.pre_depth = 0     # >0 while inside <pre>
        self.list_stack = []   # 'ul' | 'ol' nesting, with counters
        self.row = None        # cells of the table row being built
        self.cell = None       # text chunks of the cell being built
        self.is_header_row = False
        self.table_rows = []   # accumulated rows of the current table
        self.table_has_header = False
        self.href = None
        self.link_text = None

    # ---- helpers -------------------------------------------------------
    def emit(self, s):
        if self.cell is not None:
            self.cell.append(s)
        else:
            self.out.append(s)

    def text_so_far(self):
        return "".join(self.out)

    def ensure_blank_line(self):
        if self.cell is not None:
            return
        t = self.text_so_far()
        if not t:
            return
        if not t.endswith("\n\n"):
            self.out.append("\n" if t.endswith("\n") else "\n\n")

    # ---- tag handling --------------------------------------------------
    def handle_starttag(self, tag, attrs):
        a = dict(attrs)
        if tag in SKIP_TREE:
            self.skip_depth += 1
            return
        if self.skip_depth:
            return

        if tag == "br":
            self.emit("  \n" if self.pre_depth == 0 else "\n")
        elif tag in ("h1", "h2", "h3", "h4", "h5", "h6"):
            self.ensure_blank_line()
            self.emit("#" * int(tag[1]) + " ")
        elif tag == "p":
            self.ensure_blank_line()
        elif tag in ("strong", "b"):
            self.emit("**")
        elif tag in ("em", "i"):
            self.emit("*")
        elif tag == "code" and self.pre_depth == 0:
            self.emit("`")
        elif tag == "pre":
            self.ensure_blank_line()
            self.emit("```\n")
            self.pre_depth += 1
        elif tag == "sup":
            self.emit("^")
        elif tag == "sub":
            self.emit("_")
        elif tag in ("ul", "ol"):
            self.ensure_blank_line()
            self.list_stack.append([tag, 0])
        elif tag == "li":
            if self.list_stack:
                kind, n = self.list_stack[-1]
                indent = "  " * (len(self.list_stack) - 1)
                if kind == "ol":
                    n += 1
                    self.list_stack[-1][1] = n
                    self.emit(f"\n{indent}{n}. ")
                else:
                    self.emit(f"\n{indent}- ")
        elif tag == "dl":
            self.ensure_blank_line()
        elif tag == "dt":
            self.emit("\n\n**")
        elif tag == "dd":
            self.emit("\n  ")
        elif tag == "table":
            self.ensure_blank_line()
            self.table_rows = []
            self.table_has_header = False
        elif tag == "caption":
            self.emit("\n*")
        elif tag == "tr":
            self.row = []
            self.is_header_row = False
        elif tag in ("td", "th"):
            self.cell = []
            if tag == "th":
                self.is_header_row = True
        elif tag == "a":
            self.href = a.get("href")
            self.link_text = []
            self.emit("[")
        elif tag == "img":
            alt = a.get("alt") or a.get("src", "").rsplit("/", 1)[-1] or "image"
            self.emit(f"![{alt}]()")

    def handle_endtag(self, tag):
        if tag in SKIP_TREE:
            self.skip_depth = max(0, self.skip_depth - 1)
            return
        if self.skip_depth:
            return

        if tag in ("h1", "h2", "h3", "h4", "h5", "h6", "p"):
            self.emit("\n\n")
        elif tag in ("strong", "b"):
            self.emit("**")
        elif tag in ("em", "i"):
            self.emit("*")
        elif tag == "code" and self.pre_depth == 0:
            self.emit("`")
        elif tag == "pre":
            self.pre_depth = max(0, self.pre_depth - 1)
            self.emit("\n```\n\n")
        elif tag in ("ul", "ol"):
            if self.list_stack:
                self.list_stack.pop()
            self.emit("\n\n")
        elif tag == "dt":
            self.emit("**")
        elif tag == "caption":
            self.emit("*\n\n")
        elif tag in ("td", "th"):
            if self.cell is not None and self.row is not None:
                txt = "".join(self.cell)
                txt = re.sub(r"\s+", " ", txt).strip().replace("|", "\\|")
                self.row.append(txt)
            self.cell = None
        elif tag == "tr":
            if self.row:
                self.table_rows.append((self.is_header_row, self.row))
            self.row = None
        elif tag == "table":
            self.flush_table()
        elif tag == "a":
            text = "".join(self.link_text or [])
            self.emit("]")
            self.emit(f"({self.href})" if self.href else "()")
            self.href = None
            self.link_text = None

    def flush_table(self):
        if not self.table_rows:
            return
        width = max(len(r) for _, r in self.table_rows)
        rows = [(h, r + [""] * (width - len(r))) for h, r in self.table_rows]
        header, body = None, rows
        if rows[0][0]:
            header, body = rows[0][1], rows[1:]
        else:
            header, body = [""] * width, rows
            body = [r for _, r in rows]
            body = body
        self.emit("\n")
        self.emit("| " + " | ".join(header) + " |\n")
        self.emit("|" + "|".join([" --- "] * width) + "|\n")
        for item in body:
            r = item[1] if isinstance(item, tuple) else item
            self.emit("| " + " | ".join(r) + " |\n")
        self.emit("\n")
        self.table_rows = []

    def handle_data(self, data):
        if self.skip_depth:
            return
        if self.pre_depth:
            self.emit(data)
            return
        if not data.strip():
            # collapse inter-tag whitespace to a single space
            if self.cell is not None:
                self.cell.append(" ")
            elif self.out and not self.text_so_far().endswith((" ", "\n")):
                self.out.append(" ")
            return
        text = re.sub(r"\s+", " ", data)
        if self.link_text is not None:
            self.link_text.append(text)
        self.emit(text)


def tidy(md: str) -> str:
    md = html.unescape(md)
    md = re.sub(r"[ \t]+\n", "\n", md)          # trailing spaces (keep md breaks below)
    md = re.sub(r"\n{3,}", "\n\n", md)          # collapse blank runs
    md = re.sub(r"\*\*\s*\*\*", "", md)         # empty bold
    md = re.sub(r"\*\s*\*", "", md)             # empty italic
    md = re.sub(r"\[\]\([^)]*\)", "", md)         # empty anchors from <a name=...>
    md = re.sub(r"[ \t]{2,}", " ", md)          # runs of spaces
    md = re.sub(r"\n +", "\n", md)              # leading indent noise
    return md.strip() + "\n"


def main():
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("input")
    ap.add_argument("-o", "--output", default="-")
    args = ap.parse_args()

    raw = open(args.input, encoding="utf-8", errors="replace").read()
    p = SpecToMarkdown()
    p.feed(raw)
    p.close()
    md = tidy("".join(p.out))

    if args.output == "-":
        sys.stdout.write(md)
    else:
        with open(args.output, "w", encoding="utf-8") as f:
            f.write(md)
        print(f"wrote {args.output}: {len(md)} bytes, {md.count(chr(10))} lines",
              file=sys.stderr)


if __name__ == "__main__":
    main()
