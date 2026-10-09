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
    settings: Settings = Settings()
    #: Callables applied to every item in order, sync or async. Each returns
    #: the item (changed or not), or None to drop it.
    pipelines: list[Callable[[Any], Any]] = []

    async def start(self) -> AsyncIterator[Request]:
        for url in self.start_urls:
            yield Request(url)

    def parse(self, page: Page) -> Any:
        raise NotImplementedError(f"{type(self).__name__} needs a parse(self, page) method")

    def run(self, output: str | None = None) -> dict[str, int]:
        """Crawls to the end and returns the stats. ``output`` writes every
        item to a .jsonl, .csv or .parquet file."""
        return asyncio.run(self.crawl(output))

    async def crawl(self, output: str | None = None) -> dict[str, int]:
        from netweir import export

        pipelines = list(self.pipelines)
        if output is not None:
            pipelines.append(export.to_path(output))
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
        self.counts = {"items": 0, "items_dropped": 0, "callback_errors": 0, "invalid_urls": 0}

    def submit(self, request: Request) -> None:
        rid = next(self.ids)
        outcome = self.engine.submit(
            rid, request.url, request.priority, _pairs(request.headers), request.dont_filter
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
        async for request in self.spider.start():
            self.submit(request)
        while True:
            events = await self.engine.next(256)
            if not events:
                break
            for kind, rid, detail in events:
                request = self.waiting.pop(rid)
                if kind == "fetched":
                    page = Page(detail, request)
                    callback = self.resolve(request.callback, "parse")
                    await self.run_callback(callback, (page,), request.url)
                elif kind == "failed":
                    errback = self.resolve(request.errback, None)
                    if errback is None:
                        log.warning("%s: %s", request.url, detail)
                    else:
                        await self.run_callback(errback, (request, detail), request.url)
                else:
                    log.info(
                        "skipped %s: %s",
                        request.url,
                        "robots.txt" if detail == "robots" else "TDM reserved",
                    )
        stats = {**self.engine.stats(), **self.counts}
        stats.pop("queued", None)
        stats.pop("in_flight", None)
        log.info(
            "crawl finished in %.1fs: %s",
            time.monotonic() - started,
            ", ".join(f"{k} {v}" for k, v in stats.items()),
        )
        return stats

    async def run_callback(self, fn: Callable[..., Any], args: tuple, url: str) -> None:
        try:
            result = fn(*args)
            if inspect.isasyncgen(result):
                async for out in result:
                    await self.handle(out)
            else:
                if inspect.isawaitable(result):
                    result = await result
                if result is not None:
                    for out in (
                        result
                        if isinstance(result, Iterable) and not isinstance(result, (dict, str))
                        else [result]
                    ):
                        await self.handle(out)
        except Exception:
            self.counts["callback_errors"] += 1
            if self.settings.fail_fast:
                raise
            log.exception("error in %s for %s", getattr(fn, "__name__", fn), url)

    async def handle(self, out: Any) -> None:
        if out is None:
            return
        if isinstance(out, Request):
            self.submit(out)
            return
        item = out
        for stage in self.pipelines:
            item = stage(item)
            if inspect.isawaitable(item):
                item = await item
            if item is None:
                self.counts["items_dropped"] += 1
                return
        self.counts["items"] += 1
