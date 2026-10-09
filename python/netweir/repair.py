"""Repair proposals for tracked selectors, from a language model.

When a tracked selector stops matching and similarity can't find the element
(or finds it, which still means the selector is broken), the page goes to a
model with the old selector and what the element used to look like, and the
model proposes a new selector::

    class Books(netweir.Spider):
        repair = netweir.repair.llm(model="claude-opus-5-5", report="repairs.jsonl")

Every proposal is checked on the page: does it parse, what does it find, and
how much is that like the old element. Proposals are logged, kept on
``spider.repairs`` and, with ``report``, appended to a JSON Lines file. None
is ever applied: changing the spider is yours to decide.

Any model will do: ``complete`` is a function that takes the prompt and
returns the reply. ``model`` is for the default, Anthropic's API (``pip
install anthropic``, and ``ANTHROPIC_API_KEY`` set).

What is sent: the page's HTML with scripts, styles and comments removed (up
to 40,000 characters), its URL, the old selector and what the element used
to look like. Pages can hold things you'd rather not send (form tokens,
personal data); pass a ``complete`` that redacts them, or runs a local
model, if that matters. The report file records the full URL.
"""

from __future__ import annotations

import dataclasses
import json
import logging
import re
from collections.abc import Callable
from typing import Any

log = logging.getLogger("netweir")

#: Enough of a page to find an element in, without paying for all of it.
PAGE_LIMIT = 40_000


@dataclasses.dataclass
class Proposal:
    """A replacement for a broken tracked selector, not yet applied."""

    site: str
    name: str
    url: str
    old_query: str
    kind: str
    selector: str
    reason: str
    #: What the proposed selector finds on the page (the first value), or
    #: None if it finds nothing.
    matches: str | None
    #: How much its element looks like the one the old selector found, 0 to
    #: 1, or None if it finds no element.
    score: float | None


def anthropic(
    model: str = "claude-opus-5-5",
    max_tokens: int = 1024,
    client: Any = None,
    timeout: float = 120.0,
) -> Callable[[str], str]:
    """A ``complete`` function that asks Anthropic's API, giving up after
    ``timeout`` seconds."""
    if client is None:
        try:
            from anthropic import Anthropic
        except ImportError:
            raise ImportError(
                "netweir.repair.anthropic needs the anthropic package: pip install anthropic"
            ) from None
        client = Anthropic()

    def complete(prompt: str) -> str:
        message = client.messages.create(
            model=model,
            max_tokens=max_tokens,
            messages=[{"role": "user", "content": prompt}],
            timeout=timeout,
        )
        return "".join(b.text for b in message.content if getattr(b, "type", "") == "text")

    return complete


@dataclasses.dataclass
class Job:
    site: str
    name: str
    kind: str
    query: str
    fingerprint: str | None
    html: str
    url: str


class llm:  # noqa: N801 - reads like a function where it's used
    """Asks a model for replacement selectors; see the module docstring."""

    def __init__(
        self,
        model: str = "claude-opus-5-5",
        complete: Callable[[str], str] | None = None,
        report: str | None = None,
    ):
        self.model = model
        self.report = report
        self._complete = complete

    def complete(self, prompt: str) -> str:
        if self._complete is None:
            self._complete = anthropic(self.model)
        return self._complete(prompt)

    def propose(self, job: Job) -> Proposal | None:
        """Asks for a selector, checks it, and returns it as a Proposal;
        None (with a warning) when the reply has nothing usable."""
        reply = self.complete(prompt(job))
        answer = _first_json_object(reply)
        selector = answer.get("selector") if answer else None
        kind = answer.get("kind", job.kind) if answer else job.kind
        if not isinstance(selector, str) or not selector.strip() or kind not in ("css", "xpath"):
            log.warning("%s on %s: the model gave no usable selector", job.name, job.site)
            return None
        matches, score = _check(job, kind, selector)
        proposal = Proposal(
            site=job.site,
            name=job.name,
            url=job.url,
            old_query=job.query,
            kind=kind,
            selector=selector,
            reason=str(answer.get("reason", "")),
            matches=matches,
            score=score,
        )
        log.warning(
            "%s on %s: proposed %s %r (finds %r%s); not applied",
            job.name,
            job.site,
            kind,
            selector,
            matches,
            f", similarity {score:.2f}" if score is not None else "",
        )
        if self.report:
            with open(self.report, "a", encoding="utf-8") as f:
                f.write(json.dumps(dataclasses.asdict(proposal), ensure_ascii=False) + "\n")
        return proposal


def prompt(job: Job) -> str:
    was = ""
    if job.fingerprint:
        fp = json.loads(job.fingerprint)
        was = (
            "Before it broke, the element was:\n"
            f"- tag: {fp.get('tag')}\n- text: {fp.get('text')!r}\n- class: {fp.get('class')!r}\n"
            f"- inside: {' > '.join(fp.get('path', []))}\n"
            f"- text just before it: {fp.get('preceding')!r}\n"
        )
    ending = "Keep the same ending (::text or ::attr(...), or /text() or /@attr)."
    return (
        f"A web scraper's {job.kind} selector named {job.name!r} no longer finds its "
        f"element on {job.url}.\n\nThe old selector: {job.query}\n\n{was}\n"
        "Here is the page now (scripts and styles removed):\n\n"
        f"{_trim(job.html)}\n\n"
        f"Propose a selector that finds the same element on this page. {ending} "
        'Reply with one JSON object and nothing else: {"kind": "css" or "xpath", '
        '"selector": "...", "reason": "one sentence"}'
    )


def _trim(html: str) -> str:
    html = re.sub(r"(?is)<(script|style|noscript|template)\b.*?</\1\s*>", "", html)
    html = re.sub(r"(?s)<!--.*?-->", "", html)
    html = re.sub(r"\s+", " ", html)
    return html[:PAGE_LIMIT]


def _first_json_object(text: str) -> dict | None:
    """The first {...} in the reply that parses as a JSON object."""
    decoder = json.JSONDecoder()
    for start in (m.start() for m in re.finditer(r"\{", text)):
        try:
            value, _ = decoder.raw_decode(text, start)
        except ValueError:
            continue
        if isinstance(value, dict):
            return value
    return None


def _check(job: Job, kind: str, selector: str) -> tuple[str | None, float | None]:
    from netweir import parse
    from netweir._native import similarity

    root = parse(job.html)
    try:
        found = root.css(selector) if kind == "css" else root.xpath(selector)
    except ValueError:
        return None, None
    score = None
    if job.fingerprint:
        try:
            score = similarity(root, kind, selector, job.fingerprint)
        except ValueError:
            score = None
    return found.get(), score
