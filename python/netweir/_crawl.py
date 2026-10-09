"""Spiders and the crawl loop.

The Rust engine owns the queue, deduplication, robots.txt, TDMRep and
throttling; this loop hands it requests, takes batches of results back and
runs your callbacks and pipelines on them.
"""

from __future__ import annotations

import asyncio
import dataclasses
import hashlib
import inspect
import itertools
import json
import logging
import os
import time
from collections.abc import AsyncIterator, Callable, Iterable
from typing import Any

from netweir import _track
from netweir._fetch import Page, _pairs
from netweir._native import Crawler, TrackStore, fingerprint
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
    #: Further tries after a server error, a network error, throttling or
    #: a block. Retry n waits a random time up to backoff_base * 2**n
    #: seconds, capped at backoff_max.
    retries: int = 3
    backoff_base: float = 1.0
    backoff_max: float = 60.0
    #: Proxies a host moves through each time it blocks a session; every
    #: new session also starts with an empty cookie jar.
    proxies: tuple[str, ...] = ()
    #: If more than breaker_ratio of a host's last breaker_window responses
    #: were blocks, it pauses for breaker_pause seconds.
    breaker_window: int = 50
    breaker_ratio: float = 0.3
    breaker_pause: float = 300.0
    #: A directory to keep the crawl's state in. Rerunning the same crawl
    #: with the same directory resumes it: what was finished isn't fetched
    #: again, and items carry an ``_id`` so none is written twice. Delete
    #: the directory to start over.
    checkpoint: str | None = None
    #: How similar (0 to 1) an element must be to count as a tracked
    #: selector's element after the selector stops matching.
    track_threshold: float = 0.75
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
        if self.retries < 0 or self.breaker_window < 0:
            raise ValueError("retries and breaker_window must be 0 or more")
        if not 0 < self.backoff_base <= self.backoff_max or self.breaker_pause < 0:
            raise ValueError("backoffs must be positive, with backoff_base <= backoff_max")
        if not 0 <= self.breaker_ratio <= 1:
            raise ValueError("breaker_ratio must be between 0 and 1")
        if not 0 < self.track_threshold <= 1:
            raise ValueError("track_threshold must be above 0 and at most 1")
        # A list is fine to pass; stored as a tuple, as Settings is frozen.
        object.__setattr__(self, "proxies", tuple(self.proxies))

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
            retries=self.retries,
            backoff_base=self.backoff_base,
            backoff_max=self.backoff_max,
            proxies=list(self.proxies),
            breaker_window=self.breaker_window,
            breaker_ratio=self.breaker_ratio,
            breaker_pause=self.breaker_pause,
            checkpoint=self._checkpoint_file(),
        )

    def _checkpoint_file(self) -> str | None:
        if self.checkpoint is None:
            return None
        return os.path.join(self.checkpoint, "crawl.sqlite3")


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

    def on_block(self, request: Request, page: Page) -> Any:
        """Called when bot protection still blocked ``request`` after every
        retry, each with a new session. ``page.blocked`` names the vendor.
        Like a callback, it may yield items and Requests. By default it
        logs a warning."""
        kind = page._classified()[2]
        log.warning("%s: blocked by %s (%s); giving up", request.url, page.blocked, kind)

    def run(self, output: Any = None) -> dict[str, int]:
        """Crawls to the end and returns the stats. ``output`` writes every
        item to a .jsonl, .csv or .parquet file (or is an exporter)."""
        return asyncio.run(self.crawl(output))

    async def crawl(self, output: Any = None) -> dict[str, int]:
        from netweir import export

        pipelines = list(self.pipelines)
        if output is not None:
            pipelines.append(export.to_path(output) if isinstance(output, str) else output)
        run = None
        try:
            run = _Run(self, pipelines)
            # None, or how long each output file was when its items were last
            # recorded: a resumed run cuts it back to that, dropping anything
            # half-written or not recorded (which the run makes again), and
            # adds to it.
            files = run.resume()
            for stage in pipelines:
                if not isinstance(stage, export._Exporter):
                    continue
                if isinstance(stage, export.parquet):
                    # Written whole at the end; a resumed run adds a part.
                    stage.start(append=files is not None)
                    continue
                size = (files or {}).get(os.path.abspath(stage.path))
                if size is not None and os.path.exists(stage.path):
                    with open(stage.path, "r+b") as f:
                        f.truncate(size)
                stage.start(append=size is not None)
            stats = await run.go()
            run.completed = True
            return stats
        finally:
            for stage in pipelines:
                close = getattr(stage, "close", None)
                if callable(close):
                    close()
            if run is not None:
                run.finish()


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
        #: Items already delivered by an earlier run of this crawl, by _id.
        self.delivered: set[str] = set()
        self.saving = self.settings.checkpoint is not None
        #: Ids of items delivered but not yet recorded as such.
        self.unrecorded: list[str] = []
        self.completed = False
        #: Where items being handled come from, for their _id.
        self.source = ""
        self.position = 0
        self.warned: set[str] = set()
        #: Depth of requests the running callback yields.
        self.depth = 0
        # Tracked selectors keep fingerprints in the checkpoint file, or in
        # the default store.
        state = self.settings._checkpoint_file()
        store = TrackStore(state) if state else _track.context()[0]
        self.tracks = (store, self.settings.track_threshold)
        self.engine.set_tracks(store, self.settings.track_threshold)
        self.counts = {
            "items": 0,
            "items_dropped": 0,
            "callback_errors": 0,
            "invalid_urls": 0,
            "pages_ignored": 0,
            "items_already_written": 0,
            "relocated": 0,
            "lost": 0,
        }

    def resume(self) -> dict[str, int] | None:
        """Queues what an earlier run of this crawl left unfinished, and
        returns the output files' lengths as last recorded (None without a
        checkpoint)."""
        saved = self.engine.resume()
        if saved is None:
            return None
        self.delivered = set(saved["items"])
        for row, url, priority, headers, dont_filter, depth, payload in saved["pending"]:
            data = json.loads(payload)
            request = Request(
                url,
                callback=data.get("callback"),
                priority=priority,
                headers=headers,
                meta=data.get("meta", {}),
                dont_filter=dont_filter,
                errback=data.get("errback"),
                depth=depth,
            )
            self.submit(request, row=row)
        if saved["pending"] or self.delivered:
            log.info(
                "resuming: %d requests left, %d items already written",
                len(saved["pending"]),
                len(self.delivered),
            )
        state = json.loads(saved["counters"]) if saved["counters"] else {}
        return state.get("files", {})

    def payload(self, request: Request) -> str:
        """The request's own part, saved in the checkpoint as JSON:
        callbacks by name, and meta."""
        if not self.saving:
            return "{}"
        try:
            meta = json.dumps(request.meta)
        except TypeError as e:
            raise TypeError(f"with a checkpoint, meta must be JSON: {e}") from None
        callback = json.dumps(self.method_name(request.callback))
        errback = json.dumps(self.method_name(request.errback))
        return f'{{"callback": {callback}, "errback": {errback}, "meta": {meta}}}'

    def method_name(self, fn: Callable[..., Any] | str | None) -> str | None:
        if fn is None or isinstance(fn, str):
            return fn
        name = getattr(fn, "__name__", None)
        if name is not None and getattr(self.spider, name, None) == fn:
            return name
        raise TypeError(
            f"with a checkpoint, callbacks are saved by name, so {fn!r} must be a"
            " method of the spider"
        )

    def submit(self, request: Request, row: int | None = None) -> None:
        payload = self.payload(request) if row is None else "{}"
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
            payload=payload,
            row=row,
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
        # page.css(track=...) in callbacks uses this crawl's store.
        token = _track._crawl.set(self.tracks)
        try:
            return await self.crawl()
        finally:
            _track._crawl.reset(token)

    async def crawl(self) -> dict[str, int]:
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
            self.settle()
        stats = {**self.engine.stats(), **self.counts}
        stats.pop("queued", None)
        stats.pop("in_flight", None)
        log.info(
            "crawl finished in %.1fs: %s",
            time.monotonic() - started,
            ", ".join(f"{k} {v}" for k, v in stats.items()),
        )
        return stats

    def settle(self) -> None:
        """Every event in the batch has been dealt with. Once the files its
        items went to are on disk, record the items as delivered and the
        requests as done, in that order, so a crash never loses an item:
        at worst, a resume writes one again."""
        if self.saving:
            exporters = [s for s in self.pipelines if hasattr(s, "flush")]
            if not all(stage.flush() for stage in exporters):
                # Not on disk until closed (Parquet): nothing is final yet.
                return
            self.record()
        self.engine.ack()

    def record(self) -> None:
        """Records the delivered items, and how long each output file is
        with them in it, in one commit."""
        files = {
            os.path.abspath(stage.path): os.path.getsize(stage.path)
            for stage in self.pipelines
            if hasattr(stage, "flush") and os.path.exists(getattr(stage, "path", ""))
        }
        self.engine.settle(self.unrecorded, json.dumps({"files": files}))
        self.unrecorded = []

    def finish(self) -> None:
        """After the exporters are closed: the items they hold are final.
        The requests are done only if the crawl got to the end; one cut
        short leaves its last batch to be fetched again."""
        if self.saving:
            self.record()
        if self.completed:
            self.engine.ack()
        self.engine.flush()

    async def dispatch(self, event: tuple) -> None:
        kind = event[0]
        if kind == "item":
            _, rule, item, invalid, url, notes = event
            item_class = self.rules[rule].extract
            _warn_invalid(invalid, self.warned)
            for field, what, score in notes:
                self.counts["relocated" if what == "relocated" else "lost"] += 1
                _track.report(field, _track.site_of(url), what, score)
            self.source, self.position = url, 0
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
        elif kind == "blocked":
            _, rid, rule, url, _vendor, _kind, response = event
            if rule is None:
                request = self.waiting.pop(rid)
            else:
                request = Request(url, callback=self.rules[rule].callback)
            page = Page(response, request)
            await self.run_callback(self.spider.on_block, None, (request, page), url)
        elif kind == "paused":
            _, host, seconds = event
            log.warning(
                "%s keeps blocking: pausing it for %.0f s (the circuit breaker)", host, seconds
            )
        else:
            _, rid, detail, *rest = event
            request = self.waiting.pop(rid)
            if kind == "fetched":
                page = Page(detail, request, rest[0])
                if page.outcome == "payment_required":
                    price = page._classified()[4]
                    log.warning(
                        "%s: payment required%s; not retried",
                        page.url,
                        f" ({price})" if price else "",
                    )
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
        self.source = source.url if isinstance(source, (Page, Request)) else url
        self.position = 0
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
        item_id = None
        if self.saving:
            # Stable across runs: the page it came from and its place among
            # that page's items.
            key = f"{fingerprint(self.source) or self.source}:{self.position}"
            self.position += 1
            item_id = hashlib.sha256(key.encode()).hexdigest()[:24]
            if item_id in self.delivered:
                self.counts["items_already_written"] += 1
                return
            if isinstance(item, dict):
                item.setdefault("_id", item_id)
        for stage in self.pipelines:
            item = stage(item)
            if inspect.isawaitable(item):
                item = await item
            if item is None:
                self.counts["items_dropped"] += 1
                return
        self.counts["items"] += 1
        if item_id is not None:
            self.delivered.add(item_id)
            self.unrecorded.append(item_id)


#: Iterable, but one result rather than many.
_NOT_MANY = (dict, str, bytes, bytearray)
#: Never an item.
_NOT_ITEMS = (str, bytes, bytearray, int, float, bool, set, frozenset)


def _why(reason: str) -> str:
    return "robots.txt" if reason == "robots" else "TDM reserved"
