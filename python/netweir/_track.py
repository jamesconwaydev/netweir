"""Tracked selectors: where fingerprints are kept, and what's said when an
element had to be found again.

``page.css(query, track="price")`` saves what the element looks like each
time the query finds it. When a redesign breaks the query, the element most
like the saved one is used instead, if it scores at least the threshold
(``Settings.track_threshold``, 0.75), and a warning says so. In a crawl
with a checkpoint, fingerprints live in the checkpoint file; otherwise in
``~/.netweir/tracks.db`` (``$NETWEIR_HOME/tracks.db`` if that's set).
"""

from __future__ import annotations

import contextvars
import logging
import os
from typing import Any
from urllib.parse import urlsplit

from netweir._native import TrackStore

log = logging.getLogger("netweir")

THRESHOLD = 0.75

_default: TrackStore | None = None
#: The store and threshold of the crawl running in this context, if any.
_crawl: contextvars.ContextVar[tuple[TrackStore, float] | None] = contextvars.ContextVar(
    "netweir_tracks", default=None
)
#: The running crawl's way to ask for a repair: (site, name, kind, query,
#: html, url) -> None.
_repairs: contextvars.ContextVar[Any] = contextvars.ContextVar("netweir_repairs", default=None)


def default_path() -> str:
    home = os.environ.get("NETWEIR_HOME") or os.path.join(os.path.expanduser("~"), ".netweir")
    return os.path.join(home, "tracks.db")


def context() -> tuple[TrackStore, float]:
    """The store and threshold in force: the running crawl's, or the
    default store's."""
    current = _crawl.get()
    if current is not None:
        return current
    global _default
    if _default is None:
        _default = TrackStore(default_path())
    return _default, THRESHOLD


def _reset() -> None:
    """Forgets the default store (tests point NETWEIR_HOME elsewhere)."""
    global _default
    _default = None
    _reported.clear()


def site_of(url: str | None) -> str:
    return (urlsplit(url).hostname or "") if url else ""


def tracked(node: Any, kind: str, query: str, name: str, site: str, url: str = "") -> Any:
    """``node.css(query, track=name)`` or the XPath equivalent, on ``site``."""
    store, threshold = context()
    found = node.tracked(kind, query, name, store, site, threshold)
    broken = found.relocated or (found.score is not None and found.score < 1.0)
    if found.relocated:
        report(name, site, "relocated", found.score, query)
    elif broken:
        report(name, site, "lost", found.score, query)
    ask = _repairs.get()
    if broken and ask is not None:
        ask(site, name, kind, query, node.html, url)
    return found


#: (site, name, what) already reported: once is enough to know.
_reported: set[tuple[str, str, str]] = set()


def report(name: str, site: str, what: str, score: float | None, query: str = "") -> None:
    if (site, name, what) in _reported:
        return
    _reported.add((site, name, what))
    where = f" on {site}" if site else ""
    shown = f" ({query!r} no longer matches)" if query else ""
    if what == "relocated":
        log.warning("%s%s: found by similarity, score %.2f%s", name, where, score or 0.0, shown)
    else:
        log.warning(
            "%s%s: not found; the most similar element scored %.2f%s",
            name,
            where,
            score or 0.0,
            shown,
        )
