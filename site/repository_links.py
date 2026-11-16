"""Links in the repository's Markdown, rewritten for the site.

The site renders the repository's own Markdown, staged by `just site-build`
under site/pages/, so a link between files keeps the form that works on the
forge (`job-plans.md#anchor`, `../README.md`). This Markdown extension
rewrites a link to a rendered page as one relative to the page's staged
copy, which Zensical then resolves, and a link to any other repository file
to `SITE_SOURCE_URL` (the prefix of a repository file's URL, ending in `/`):
the forge's view of the built commit when deployed, the checkout itself
otherwise, so a local link check also proves those files exist.
"""

from __future__ import annotations

import os
import posixpath
import re
from pathlib import Path
from urllib.parse import quote, unquote

from markdown import Extension
from markdown.treeprocessors import Treeprocessor
from zensical.extensions.context import ContextPreprocessor

# Repository files the site renders beside docs/*.md, by staged path; the
# staging in the Justfile's site-build and this table must agree.
PAGES = {"site/index.md": "index.md"}
DOCS = "docs/"
ROOT = Path(__file__).resolve().parent.parent
SCHEME = re.compile(r"^[a-z][a-z0-9+.-]*:", re.IGNORECASE)


def staged(file: str) -> str | None:
    """The staged path of a repository file the site renders, or None."""
    if file in PAGES:
        return PAGES[file]
    name = file.removeprefix(DOCS)
    if name != file and name.endswith(".md") and "/" not in name:
        return file
    return None


def repository_file(path: str) -> str:
    """The repository file a staged page was copied from."""
    for file, page in PAGES.items():
        if page == path:
            return file
    return path


def rewrite(url: str, page: str, source: str) -> str:
    """`url`, written in the staged page `page`, rewritten for the site.

    External, absolute and fragment-only links are kept.
    """
    if SCHEME.match(url) or url.startswith(("/", "#")) or url == "":
        return url
    target, hash, fragment = url.partition("#")
    origin = repository_file(page)
    file = posixpath.normpath(posixpath.join(posixpath.dirname(origin), unquote(target)))
    # A link out of the repository is not one this rewriting can place.
    if file == ".." or file.startswith("../"):
        return url
    to = staged(file)
    if to is not None:
        return posixpath.relpath(to, posixpath.dirname(page) or ".") + hash + fragment
    if file == ".":
        file = ""
    elif target.endswith("/"):
        file += "/"
    return source + quote(file) + hash + fragment


class RepositoryLinks(Treeprocessor):
    def run(self, root):
        context = ContextPreprocessor.from_markdown(self.md)
        if context is None:
            return
        page = context.page.path.replace(os.sep, "/")
        source = os.environ.get("SITE_SOURCE_URL") or ROOT.as_uri() + "/"
        for link in root.iter("a"):
            href = link.get("href")
            if href is not None:
                link.set("href", rewrite(href, page, source))


class RepositoryLinksExtension(Extension):
    def extendMarkdown(self, md):
        md.treeprocessors.register(RepositoryLinks(md), "repository_links", 0)


def makeExtension(**kwargs):
    return RepositoryLinksExtension(**kwargs)
