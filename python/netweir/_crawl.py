"""Spiders and the crawl loop.

The Rust engine owns the queue, deduplication, robots.txt, TDMRep and
throttling; this loop hands it requests, takes batches of results back and
runs your callbacks and pipelines on them.
"""

from __future__ import annotations

import asyncio
import dataclasses
import inspect
import itertools
import logging
import time
from collections.abc import AsyncIterator, Callable, Iterable
from typing import Any

from netweir._fetch import Page, _pairs
from netweir._native import Crawler
from netweir._request import Request
from netweir._rules import Follow, _warn_invalid

log = logging.getLogger("netweir")


@dataclasses.dataclass(frozen=True)
class Settings:
    """How a crawl behaves. Every field has a polite default."""

    profile: str = "chrome"
    proxy: str | None = None
    #: Seconds for a whole request.
    timeout: float = 30.0
    #: Requests in flight across all sites, and to one host.
    concurrency: int = 64
    per_domain: int = 8
    obey_robots: bool = True
    #: The name matched against robots.txt User-agent lines.
    robots_agent: str = "netweir"
    #: Skip pages whose site reserves text and data mining rights (TDMRep).
    obey_tdmrep: bool = True
    #: Adapt each host's delay to how fast it answers.
    throttle: bool = True
    start_delay: float = 1.0
    min_delay: float = 0.0
    max_delay: float = 60.0
    target_concurrency: float = 1.0
    #: Links followed from a start page beyond which requests are dropped.
    max_depth: int | None = None
    #: Requests accepted for one host beyond which more are dropped.
    max_pages_per_domain: int | None = None
    #: Stop the crawl at the first exception in a callback or pipeline.
    fail_fast: bool = False

    def __post_init__(self):
        if self.concurrency < 1 or self.per_domain < 1:
            raise ValueError("concurrency and per_domain must be at least 1")
        if self.timeout <= 0:
            raise ValueError("timeout must be positive")
        if not 0 <= self.min_delay <= self.max_delay or self.start_delay < 0:
            raise ValueError("delays must be non-negative, with min_delay <= max_delay")
        if self.target_concurrency <= 0:
            raise ValueError("target_concurrency must be positive")
        if self.max_depth is not None and self.max_depth < 0:
            raise ValueError("max_depth must be 0 or more")
        if self.max_pages_per_domain is not None and self.max_pages_per_domain < 1:
            raise ValueError("max_pages_per_domain must be at least 1")

    def _engine(self) -> Crawler:
        return Crawler(
            profile=self.profile,
            proxy=self.proxy,
            timeout=self.timeout,
            concurrency=self.concurrency,
            per_domain=self.per_domain,
            obey_robots=self.obey_robots,
            robots_agent=self.robots_agent,
            obey_tdmrep=self.obey_tdmrep,
            throttle=self.throttle,
            start_delay=self.start_delay,
            min_delay=self.min_delay,
            max_delay=self.max_delay,
            target_concurrency=self.target_concurrency,
            max_depth=self.max_depth,
            max_pages_per_domain=self.max_pages_per_domain,
        )


class Spider:
    """Subclass this. Set ``start_urls`` (or override ``start``), write
    ``parse``, and run it with ``run()`` or ``await crawl()``.

    A callback receives a Page and yields (or returns) items, which are
    dicts or dataclasses, and Requests to follow. It can be an async
    generator, a generator, a coroutine or a plain function.
    """

    name: str | None = None
    start_urls: list[str] = []
    #: Link rules (``netweir.Follow``), run in Rust. A spider made only of
    #: rules and Items runs no Python per page.
    rules: list[Follow] = []
    settings: Settings = Settings()
    #: Callables applied to every item in order, sync or async. Each returns
    #: the item (changed or not), or None to drop it.
    pipelines: list[Callable[[Any], Any]] = []

    async def start(self) -> AsyncIterator[Request]:
        """The first requests. An async generator by default; a plain
        generator or a list works too."""
        for url in self.start_urls:
            yield Request(url)

    def parse(self, page: Page) -> Any:
        raise NotImplementedError(f"{type(self).__name__} needs a parse(self, page) method")

    def run(self, output: Any = None) -> dict[str, int]:
        """Crawls to the end and returns the stats. ``output`` writes every
        item to a .jsonl, .csv or .parquet file (or is an exporter)."""
        return asyncio.run(self.crawl(output))

    async def crawl(self, output: Any = None) -> dict[str, int]:
        from netweir import export

        pipelines = list(self.pipelines)
        if output is not None:
            pipelines.append(export.to_path(output) if isinstance(output, str) else output)
        try:
            return await _Run(self, pipelines).go()
        finally:
            for stage in pipelines:
                close = getattr(stage, "close", None)
                if callable(close):
                    close()


class _Run:
    def __init__(self, spider: Spider, pipelines: list[Callable[[Any], Any]]):
        self.spider = spider
        self.pipelines = pipelines
        self.settings = spider.settings
        self.engine = self.settings._engine()
        self.ids = itertools.count()
        self.waiting: dict[int, Request] = {}
        self.rules = list(spider.rules)
        for rule in self.rules:
            self.engine.add_rule(
                rule.kind,
                rule.query,
                rule.extract._spec if rule.extract is not None else None,
                follow=rule.follow,
                to_python=rule.callback is not None,
                priority=rule.priority,
            )
        # parse runs alongside the rules only if the spider writes its own.
        self.has_parse = type(spider).parse is not Spider.parse
        self.warned: set[str] = set()
        #: Depth of requests the running callback yields.
        self.depth = 0
        self.counts = {
            "items": 0,
            "items_dropped": 0,
            "callback_errors": 0,
            "invalid_urls": 0,
            "pages_ignored": 0,
        }

    def submit(self, request: Request) -> None:
        rid = next(self.ids)
        # A request with no callback on a rules spider is the rules' to read.
        apply_rules = bool(self.rules) and request.callback is None
        to_python = not apply_rules or self.has_parse
        outcome = self.engine.submit(
            rid,
            request.url,
            request.priority,
            _pairs(request.headers),
            request.dont_filter,
            apply_rules=apply_rules,
            to_python=to_python,
            depth=request.depth,
        )
        if outcome == "queued":
            self.waiting[rid] = request
        elif outcome == "invalid":
            self.counts["invalid_urls"] += 1
            log.warning("not an http(s) URL, skipped: %s", request.url)

    def resolve(
        self, fn: Callable[..., Any] | str | None, default: str | None
    ) -> Callable[..., Any] | None:
        if fn is None:
            return getattr(self.spider, default) if default else None
        if isinstance(fn, str):
            return getattr(self.spider, fn)
        return fn

    async def go(self) -> dict[str, int]:
        started = time.monotonic()
        if not self.settings.obey_robots:
            log.warning("obey_robots is off: robots.txt is not being checked")
        start = self.spider.start()
        if inspect.isasyncgen(start):
            async for request in start:
                self.submit(request)
        else:
            for request in start:
                self.submit(request)
        while True:
            events = await self.engine.next(256)
            if not events:
                break
            for event in events:
                await self.dispatch(event)
        stats = {**self.engine.stats(), **self.counts}
        stats.pop("queued", None)
        stats.pop("in_flight", None)
        log.info(
            "crawl finished in %.1fs: %s",
            time.monotonic() - started,
            ", ".join(f"{k} {v}" for k, v in stats.items()),
        )
        return stats

    async def dispatch(self, event: tuple) -> None:
        kind = event[0]
        if kind == "item":
            _, rule, item, invalid = event
            item_class = self.rules[rule].extract
            _warn_invalid(invalid, self.warned)
            await self.handle(item_class._finish(item, self.warned))
        elif kind == "ruled":
            _, rule, url, response, root, depth = event
            callback = self.rules[rule].callback
            request = Request(url, callback=callback, depth=depth)
            page = Page(response, request, root)
            await self.run_callback(callback, None, (page,), url)
        elif kind == "rule_failed":
            _, _, url, error = event
            log.warning("%s: %s", url, error)
        elif kind == "rule_dropped":
            _, _, url, why = event
            log.info("skipped %s: %s", url, _why(why))
        elif kind == "ignored":
            _, url, reason = event
            self.counts["pages_ignored"] += 1
            log.info("rules skipped %s: %s", url, reason)
        elif kind == "page_error":
            _, url, message = event
            log.warning("%s: %s", url, message)
        elif kind == "handled":
            self.waiting.pop(event[1])
        else:
            _, rid, detail, *rest = event
            request = self.waiting.pop(rid)
            if kind == "fetched":
                page = Page(detail, request, rest[0])
                await self.run_callback(request.callback, "parse", (page,), request.url)
            elif kind == "failed":
                if request.errback is None:
                    log.warning("%s: %s", request.url, detail)
                else:
                    await self.run_callback(request.errback, None, (request, detail), request.url)
            else:
                log.info("skipped %s: %s", request.url, _why(detail))

    async def run_callback(
        self, fn: Callable[..., Any] | str | None, default: str | None, args: tuple, url: str
    ) -> None:
        name = fn if isinstance(fn, str) else getattr(fn, "__name__", default)
        # What it yields is one link further from the start.
        source = args[0]
        self.depth = (source.depth if isinstance(source, (Page, Request)) else 0) + 1
        try:
            fn = self.resolve(fn, default)
            result = fn(*args)
            if inspect.isasyncgen(result):
                async for out in result:
                    await self.handle(out)
            else:
                if inspect.isawaitable(result):
                    result = await result
                if result is not None:
                    many = isinstance(result, Iterable) and not isinstance(result, _NOT_MANY)
                    for out in result if many else [result]:
                        await self.handle(out)
        except Exception:
            self.counts["callback_errors"] += 1
            if self.settings.fail_fast:
                raise
            log.exception("error in %s for %s", name, url)

    async def handle(self, out: Any) -> None:
        if out is None:
            return
        if isinstance(out, Request):
            out.depth = self.depth
            self.submit(out)
            return
        if isinstance(out, _NOT_ITEMS):
            raise TypeError(
                f"a callback produced a {type(out).__name__}: yield dicts, dataclasses or Requests"
            )
        item = out
        for stage in self.pipelines:
            item = stage(item)
            if inspect.isawaitable(item):
                item = await item
            if item is None:
                self.counts["items_dropped"] += 1
                return
        self.counts["items"] += 1


#: Iterable, but one result rather than many.
_NOT_MANY = (dict, str, bytes, bytearray)
#: Never an item.
_NOT_ITEMS = (str, bytes, bytearray, int, float, bool, set, frozenset)


def _why(reason: str) -> str:
    return "robots.txt" if reason == "robots" else "TDM reserved"
