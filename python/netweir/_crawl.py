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
import re
import time
from collections.abc import AsyncIterator, Callable, Iterable, Iterator
from concurrent.futures.process import BrokenProcessPool
from typing import Any
from urllib.parse import urlsplit

from netweir import _track
from netweir._fetch import Page, _pairs
from netweir._native import Crawler, TrackStore, _robots_sitemaps, fingerprint
from netweir._native import _sitemap as _parse_sitemap
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
    #: Bytes of a response body, after decompression, beyond which the
    #: request fails with FetchError(kind="too_large"); None for no limit.
    max_response_size: int | None = 64 * 1024 * 1024
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
    #: The crawl stops once it has delivered this many items, sent this
    #: many requests (each retry among them), had this many errors in
    #: callbacks or pipelines, or run for this many seconds. With a
    #: checkpoint, running it again carries on from where it stopped.
    max_items: int | None = None
    max_pages: int | None = None
    max_errors: int | None = None
    max_time: float | None = None
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
    #: Processes to run callbacks in. With more than one, the spider class
    #: must be importable (defined at module level, or in the file given to
    #: ``netweir crawl``), and callbacks run in worker processes while the
    #: engine, pipelines and checkpoint stay in this one.
    workers: int = 1
    #: Which requests are fetched in Chrome: "off" (only those that ask
    #: with ``browser=True``), "on_block" (also any still blocked after its
    #: retries, once) or "always".
    browser: str = "off"
    #: Chrome pages open at once.
    browser_pages: int = 4

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
        for field in ("max_items", "max_pages", "max_errors"):
            if getattr(self, field) is not None and getattr(self, field) < 1:
                raise ValueError(f"{field} must be at least 1")
        if self.max_time is not None and self.max_time <= 0:
            raise ValueError("max_time must be positive")
        if self.retries < 0 or self.breaker_window < 0:
            raise ValueError("retries and breaker_window must be 0 or more")
        if not 0 < self.backoff_base <= self.backoff_max or self.breaker_pause < 0:
            raise ValueError("backoffs must be positive, with backoff_base <= backoff_max")
        if not 0 <= self.breaker_ratio <= 1:
            raise ValueError("breaker_ratio must be between 0 and 1")
        if not 0 < self.track_threshold <= 1:
            raise ValueError("track_threshold must be above 0 and at most 1")
        if self.browser not in ("off", "on_block", "always"):
            raise ValueError(f'browser must be "off", "on_block" or "always", not {self.browser!r}')
        if self.browser_pages < 1:
            raise ValueError("browser_pages must be at least 1")
        if self.workers < 1:
            raise ValueError("workers must be at least 1")
        # A list is fine to pass; stored as a tuple, as Settings is frozen.
        object.__setattr__(self, "proxies", tuple(self.proxies))

    def _engine(self, allowed_domains: Iterable[str] = ()) -> Crawler:
        return Crawler(
            allowed_domains=list(allowed_domains),
            profile=self.profile,
            proxy=self.proxy,
            timeout=self.timeout,
            max_response_size=self.max_response_size,
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
            max_pages=self.max_pages,
            retries=self.retries,
            backoff_base=self.backoff_base,
            backoff_max=self.backoff_max,
            proxies=list(self.proxies),
            breaker_window=self.breaker_window,
            breaker_ratio=self.breaker_ratio,
            breaker_pause=self.breaker_pause,
            checkpoint=self._checkpoint_file(),
            browser=self.browser,
            browser_pages=self.browser_pages,
        )

    def _checkpoint_file(self) -> str | None:
        if self.checkpoint is None:
            return None
        return os.path.join(self.checkpoint, "crawl.sqlite3")


class Spider:
    """Subclass this. Set ``start_urls`` (or override ``start``), write
    ``parse``, and run it with ``run()`` or ``await crawl()``.

    To crawl a site from its sitemaps, set ``sitemap_urls`` to sitemaps or
    to its robots.txt, which names them. ``sitemap_rules`` sends each page
    to a callback by the first pattern (a regex) its URL matches;
    ``sitemap_follow`` says which sitemaps of an index to read.

    A callback receives a Page and yields (or returns) items, which are
    dicts or dataclasses, and Requests to follow. It can be an async
    generator, a generator, a coroutine or a plain function.
    """

    name: str | None = None
    start_urls: list[str] = []
    #: Sitemaps (XML, gzipped or not, or text), sitemap indexes, or a
    #: robots.txt whose Sitemap lines name them.
    sitemap_urls: list[str] = []
    #: (pattern, callback) pairs: a page goes to the first whose regex its
    #: URL matches; pages none matches are skipped.
    sitemap_rules: list[tuple[str, Callable[..., Any] | str]] = [("", "parse")]
    #: Regexes for which sitemaps of an index to read: all, by default.
    sitemap_follow: list[str] = [""]
    #: Also crawl the other-language versions a sitemap lists for a page.
    sitemap_alternate_links: bool = False
    #: Link rules (``netweir.Follow``), run in Rust. A spider made only of
    #: rules and Items runs no Python per page.
    rules: list[Follow] = []
    settings: Settings = Settings()
    #: Callables applied to every item in order, sync or async. Each returns
    #: the item (changed or not), or None to drop it.
    pipelines: list[Callable[[Any], Any]] = []
    #: Domains the crawl may go to, each with its subdomains; a domain with
    #: a port allows only that port. Requests elsewhere are dropped (and
    #: counted as "offsite") unless they have dont_filter=True. Empty for
    #: anywhere.
    allowed_domains: list[str] = []
    #: Asks for replacement selectors when tracked ones break (see
    #: netweir.repair). Proposals end up in ``repairs`` after a run.
    repair: Any = None
    repairs: list[Any] = []
    #: Why the last run ended: "finished" when it ran out of requests, or
    #: the setting that stopped it ("max_items", "max_pages", "max_errors",
    #: "max_time").
    finish_reason: str | None = None

    async def start(self) -> AsyncIterator[Request]:
        """The first requests. An async generator by default; a plain
        generator or a list works too."""
        for url in self.start_urls:
            yield Request(url)
        for url in self.sitemap_urls:
            yield Request(url, callback="_sitemap")

    def sitemap_filter(self, entries: Iterable[dict[str, Any]]) -> Iterable[dict[str, Any]]:
        """Chooses which sitemap entries to crawl; all of them by default.
        Each is a dict with ``loc``, ``lastmod``, ``changefreq``,
        ``priority`` (None where the sitemap doesn't say) and
        ``alternates``. Applies to an index's entries too."""
        return entries

    def _sitemap(self, page: Page) -> Iterator[Request]:
        """Reads a sitemap, an index or a robots.txt, and yields the pages
        and sitemaps it leads to."""
        if urlsplit(page.url).path.endswith("/robots.txt"):
            for url in _robots_sitemaps(page.text):
                yield Request(page.urljoin(url), callback="_sitemap")
            return
        limit = self.settings.max_response_size or 1 << 30
        try:
            kind, entries = _parse_sitemap(page.body, limit)
        except ValueError as e:
            log.warning("%s: not read as a sitemap: %s", page.url, e)
            return
        entries = self.sitemap_filter(entries)
        if kind == "index":
            follow = [re.compile(p) for p in self.sitemap_follow]
            for entry in entries:
                if any(p.search(entry["loc"]) for p in follow):
                    yield Request(entry["loc"], callback="_sitemap")
            return
        rules = [(re.compile(p), cb) for p, cb in self.sitemap_rules]
        for entry in entries:
            urls = [entry["loc"]]
            if self.sitemap_alternate_links:
                urls += entry["alternates"]
            for url in urls:
                callback = next((cb for p, cb in rules if p.search(url)), None)
                if callback is not None:
                    yield Request(url, callback=callback)

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
        # Nothing is opened or written until this has worked: a crawl that
        # can't start leaves its output files as they were.
        run = _Run(self, pipelines)
        started: list[export._Exporter] = []
        try:
            # None, or the state of the output files when items were last
            # recorded: a resumed run cuts each file back to its recorded
            # length, dropping anything half-written or not recorded (which
            # the run makes again), and adds to it.
            state = run.resume()
            files = (state or {}).get("files", {})
            for stage in pipelines:
                if not isinstance(stage, export._Exporter):
                    continue
                if isinstance(stage, export.parquet):
                    # Written whole at the end; a resumed run adds a part.
                    run.tidy_parts(stage)
                    stage.hold = run.saving
                    stage.start(append=state is not None)
                else:
                    size = files.get(os.path.abspath(stage.path))
                    if size is not None and os.path.exists(stage.path):
                        with open(stage.path, "r+b") as f:
                            f.truncate(size)
                    stage.start(append=size is not None)
                started.append(stage)
            stats = await run.go()
            # Stopped by a limit part way through a batch: the batch is
            # fetched again by a resumed run.
            run.completed = self.finish_reason == "finished"
            return stats
        finally:
            await run.engine.close_browser()
            for stage in pipelines:
                if isinstance(stage, export._Exporter) and not any(stage is s for s in started):
                    continue
                close = getattr(stage, "close", None)
                if callable(close):
                    close()
            run.finish()


class _Stop(BaseException):
    """A limit was reached. A BaseException, so that the callback or
    pipeline it's raised under doesn't take it for its own error."""

    def __init__(self, reason: str):
        super().__init__(reason)
        self.reason = reason


def _warn_if_priced(page: Page) -> None:
    if page.outcome == "payment_required":
        price = page._classified()[4]
        log.warning("%s: payment required%s; not retried", page.url, f" ({price})" if price else "")


def _worker_died(name: str, url: str) -> RuntimeError:
    return RuntimeError(
        f"a worker process died while running {name} for {url}, or couldn't start;"
        " with a checkpoint, the crawl resumes where it stopped"
    )


class _Run:
    def __init__(self, spider: Spider, pipelines: list[Callable[[Any], Any]]):
        self.spider = spider
        self.pipelines = pipelines
        self.settings = spider.settings
        self.engine = self.settings._engine(spider.allowed_domains)
        #: Worker processes for callbacks, with Settings.workers > 1.
        self.pool: Any = None
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
                allow=rule.allow,
                deny=rule.deny,
                allow_domains=rule.allow_domains,
                deny_domains=rule.deny_domains,
            )
        # parse runs alongside the rules only if the spider writes its own.
        self.has_parse = type(spider).parse is not Spider.parse
        #: Items already delivered by an earlier run of this crawl, by _id.
        self.delivered: set[str] = set()
        self.saving = self.settings.checkpoint is not None
        #: Ids of items delivered but not yet recorded as such.
        self.unrecorded: list[str] = []
        #: Parquet parts whose items are on record, by absolute path.
        self.parts: list[str] = []
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
        spider.repairs = []
        spider.finish_reason = None
        #: When the crawl started, for max_time.
        self.started = time.monotonic()
        #: Repairs to ask for once the crawl is done, one per site and name.
        self.repair_jobs: list[Any] = []
        self.repair_asked: set[tuple[str, str]] = set()
        self.counts = {
            "items": 0,
            "items_dropped": 0,
            "callback_errors": 0,
            "invalid_urls": 0,
            "pages_ignored": 0,
            "items_already_written": 0,
            "relocated": 0,
            "lost": 0,
            "repair_proposals": 0,
        }

    def resume(self) -> dict[str, int] | None:
        """Queues what an earlier run of this crawl left unfinished, and
        returns the output files' lengths as last recorded (None without a
        checkpoint)."""
        saved = self.engine.resume()
        if saved is None or not (saved["pending"] or saved["items"] or saved["counters"]):
            # No checkpoint, or one this crawl has just created.
            return None
        self.delivered = set(saved["items"])
        for (row, url, priority, headers, dont_filter, depth, payload), sent in saved["pending"]:
            method, kind, body, content_type, referer, retry_post = sent
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
                browser=data.get("browser", False),
                method=method,
                body=body,
                referer=referer,
                retry_post=retry_post,
                kind=kind,
                content_type=content_type,
            )
            self.submit(request, row=row)
        if saved["pending"] or self.delivered:
            log.info(
                "resuming: %d requests left, %d items already written",
                len(saved["pending"]),
                len(self.delivered),
            )
        state = json.loads(saved["counters"]) if saved["counters"] else {}
        self.parts = list(state.get("parts", []))
        return state

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
        browser = json.dumps(request.browser)
        return (
            f'{{"callback": {callback}, "errback": {errback}, "meta": {meta}, '
            f'"browser": {browser}}}'
        )

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
            browser=request.browser,
            method=request.method,
            kind=request.kind,
            body=request.body.encode() if isinstance(request.body, str) else request.body,
            content_type=request.content_type,
            referer=request.referer,
            retry_post=request.retry_post,
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
        asking = _track._repairs.set(self.ask_repair if self.spider.repair else None)
        reported = _track._crawl_reported.set(set())
        try:
            return await self.crawl()
        finally:
            _track._crawl_reported.reset(reported)
            _track._repairs.reset(asking)
            _track._crawl.reset(token)

    def ask_repair(self, site: str, name: str, kind: str, query: str, html: Any, url: str):
        """Queues a repair for after the crawl. ``html`` is the page, or a
        function that gives it, called only for a repair not yet queued."""
        if self.spider.repair is None or (site, name) in self.repair_asked:
            return
        from netweir.repair import Job

        self.repair_asked.add((site, name))
        fingerprint = self.tracks[0].get(site, name)
        page = html() if callable(html) else html
        self.repair_jobs.append(Job(site, name, kind, query, fingerprint, page, url))

    async def run_repairs(self) -> None:
        """Asks for the repairs the crawl ran into, off the event loop."""
        for job in self.repair_jobs:
            try:
                proposal = await asyncio.to_thread(self.spider.repair.propose, job)
            except Exception:  # noqa: BLE001 - a failed repair never fails the crawl
                log.exception("asking for a repair of %s on %s failed", job.name, job.site)
                continue
            if proposal is not None:
                self.spider.repairs.append(proposal)
                self.counts["repair_proposals"] += 1

    def check_limits(self) -> None:
        """Raises _Stop if the crawl has reached one of its limits."""
        settings = self.settings
        if settings.max_items is not None and self.counts["items"] >= settings.max_items:
            raise _Stop("max_items")
        if (
            settings.max_errors is not None
            and self.counts["callback_errors"] >= settings.max_errors
        ):
            raise _Stop("max_errors")
        if settings.max_time is not None and self.time_left() <= 0:
            raise _Stop("max_time")

    def time_left(self) -> float | None:
        if self.settings.max_time is None:
            return None
        return self.settings.max_time - (time.monotonic() - self.started)

    async def next_events(self) -> list[tuple]:
        """The next batch, waiting no longer than max_time allows."""
        left = self.time_left()
        if left is None:
            return await self.engine.next(256)
        try:
            return await asyncio.wait_for(self.engine.next(256), max(left, 0))
        # Not the builtin TimeoutError before Python 3.11.
        except asyncio.TimeoutError:
            raise _Stop("max_time") from None

    async def crawl(self) -> dict[str, int]:
        started = self.started
        if not self.settings.obey_robots:
            log.warning("obey_robots is off: robots.txt is not being checked")
        abandoned = True
        try:
            if self.settings.workers > 1:
                from netweir import _workers

                state = self.settings._checkpoint_file()
                self.pool = _workers.Pool(
                    self.spider, self.settings, state or _track.default_path()
                )
            start = self.spider.start()
            if inspect.isasyncgen(start):
                async for request in start:
                    self.submit(request)
            else:
                for request in start:
                    self.submit(request)
            try:
                while True:
                    self.check_limits()
                    events = await self.next_events()
                    if not events:
                        # The engine stops sending at max_pages, and ends
                        # the crawl with requests still queued.
                        if self.engine.stats()["queued"]:
                            raise _Stop("max_pages")
                        break
                    # With workers, the batch's callbacks all start now; their
                    # results are dealt with below, in the batch's order.
                    jobs = [self.send(event) for event in events]
                    await self.dispatch_batch(events, jobs)
                    self.settle()
                self.spider.finish_reason = "finished"
                abandoned = False
            except _Stop as stop:
                self.spider.finish_reason = stop.reason
                log.info("stopping the crawl: %s reached", stop.reason)
        finally:
            if self.pool is not None:
                # Stopping early (fail_fast, an interrupt) doesn't wait for
                # callbacks the workers are still running.
                self.pool.close(abandon=abandoned)
                self.pool = None
        await self.run_repairs()
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
            from netweir import export

            exporters = [s for s in self.pipelines if isinstance(s, export._Exporter)]
            if not all(stage.flush() for stage in exporters):
                # Not on disk until closed (Parquet): nothing is final yet.
                return
            self.record()
        self.engine.ack()

    def record(self) -> None:
        """Records the delivered items, and how long each output file is
        with them in it, in one commit."""
        from netweir import export

        files = {
            os.path.abspath(stage.path): os.path.getsize(stage.path)
            for stage in self.pipelines
            if isinstance(stage, export._Exporter)
            and not isinstance(stage, export.parquet)
            and os.path.exists(stage.path)
        }
        parts = [
            os.path.abspath(stage.pending)
            for stage in self.pipelines
            if isinstance(stage, export.parquet) and stage.pending is not None
        ]
        self.parts.extend(p for p in parts if p not in self.parts)
        self.engine.settle(self.unrecorded, json.dumps({"files": files, "parts": self.parts}))
        self.unrecorded = []

    def tidy_parts(self, stage: Any) -> None:
        """Before a Parquet exporter starts: a part whose items were recorded
        but which the process died before naming gets its name; any other
        .partial file of this output, never recorded, is deleted (the run
        makes its items again)."""
        import glob

        stem = stage.path[: -len(".parquet")] if stage.path.endswith(".parquet") else stage.path
        for partial in glob.glob(glob.escape(stem) + "*.parquet.partial"):
            final = partial[: -len(".partial")]
            if os.path.abspath(final) in self.parts and not os.path.exists(final):
                os.replace(partial, final)
            else:
                os.remove(partial)

    def finish(self) -> None:
        """After the exporters are closed: the items they hold are final.
        The requests are done only if the crawl got to the end; one cut
        short leaves its last batch to be fetched again."""
        from netweir import export

        try:
            if self.saving:
                self.record()
            if self.completed:
                self.engine.ack()
        finally:
            # Not left to the garbage collector: a traceback can keep this
            # run alive, and Windows won't delete an open file.
            self.engine.close()
            if self.settings._checkpoint_file():
                self.tracks[0].close()
        # Only now, with their items on record, do Parquet parts get their
        # real names.
        for stage in self.pipelines:
            if isinstance(stage, export.parquet):
                stage.publish()

    async def dispatch_batch(self, events: list[tuple], jobs: list[Any]) -> None:
        """Deals with a batch's events in order. With fail_fast, a worker's
        error stops the crawl when it happens, not when its page's turn
        comes behind slower callbacks."""
        dispatching = asyncio.ensure_future(self._dispatch_all(events, jobs))
        pending = [job for job in jobs if job is not None]
        if not (self.settings.fail_fast and pending):
            await dispatching
            return
        failed: asyncio.Future = asyncio.get_running_loop().create_future()

        def check(job: asyncio.Future) -> None:
            if failed.done() or job.cancelled() or job.exception() is not None:
                return
            error = job.result()[3]
            if error is not None:
                failed.set_result(error[0])

        for job in pending:
            job.add_done_callback(check)
        await asyncio.wait({dispatching, failed}, return_when=asyncio.FIRST_COMPLETED)
        if dispatching.done():
            failed.cancel()
            dispatching.result()
            return
        dispatching.cancel()
        self.counts["callback_errors"] += 1
        raise failed.result()

    async def _dispatch_all(self, events: list[tuple], jobs: list[Any]) -> None:
        for event, job in zip(events, jobs, strict=True):
            await self.dispatch(event, job)
            self.check_limits()

    def send(self, event: tuple) -> Any:
        """Starts the event's callback in a worker, if there are workers
        and it's a page's callback that can run in one. The pending result,
        or None."""
        if self.pool is None or event[0] not in ("fetched", "ruled"):
            return None
        if event[0] == "fetched":
            _, rid, response, _root, live = event
            if live is not None:
                return None  # a live Chrome page can't leave this process
            request = self.waiting.get(rid)
            if request is None:
                return None
            callback, default = request.callback, "parse"
        else:
            _, rule, url, response, _root, depth = event
            callback, default = self.rules[rule].callback, None
            request = Request(url, callback=callback, depth=depth)
        try:
            name = self.method_name(callback) or default
            sent = dataclasses.replace(
                request, callback=name, errback=self.method_name(request.errback)
            )
        except TypeError:
            return None  # run here, where the error is reported as usual
        if name is None:
            return None
        try:
            return self.pool.submit(name, response, sent)
        except BrokenProcessPool:
            raise _worker_died(name, request.url) from None

    async def dispatch(self, event: tuple, job: Any = None) -> None:
        kind = event[0]
        if kind == "item":
            _, rule, item, invalid, url, notes, html = event
            item_class = self.rules[rule].extract
            _warn_invalid(invalid, self.warned)
            site = _track.site_of(url)
            for field, what, score in notes:
                self.counts["relocated" if what == "relocated" else "lost"] += 1
                _track.report(field, site, what, score)
                spec = item_class._fields[field.rsplit(".", 1)[1]]
                self.ask_repair(site, spec.track, spec.kind, spec.query, html or "", url)
            # Its own namespace: the page's callback numbers its items from 0
            # too.
            self.source, self.position = f"rule{rule}:{url}", 0
            await self.handle(item_class._finish(item, self.warned))
        elif kind == "ruled":
            _, rule, url, response, root, depth = event
            callback = self.rules[rule].callback
            request = Request(url, callback=callback, depth=depth)
            if job is not None:
                await self.collect(job, callback, request, response.url)
                return
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
        elif kind == "warning":
            log.warning("%s", event[1])
        elif kind == "paused":
            _, host, seconds = event
            log.warning(
                "%s keeps blocking: pausing it for %g s (the circuit breaker)", host, seconds
            )
        else:
            _, rid, detail, *rest = event
            request = self.waiting.pop(rid)
            if kind == "fetched" and job is not None:
                _warn_if_priced(Page(detail, request))
                await self.collect(job, request.callback or "parse", request, detail.url)
            elif kind == "fetched":
                page = Page(detail, request, rest[0])
                page.browser = rest[1]
                _warn_if_priced(page)
                try:
                    await self.run_callback(request.callback, "parse", (page,), request.url)
                finally:
                    # Its place in browser_pages goes to the next request.
                    if page.browser is not None:
                        await page.browser.close()
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

    async def collect(self, job: Any, fn: Any, request: Request, page_url: str) -> None:
        """A worker's results for one page, dealt with as run_callback deals
        with a callback's: same depth, same item ids, same counts."""
        name = fn if isinstance(fn, str) else getattr(fn, "__name__", "parse")
        self.depth = request.depth + 1
        self.source = page_url
        self.position = 0
        try:
            outputs, reports, repairs, error = await job
        except BrokenProcessPool:
            # One page's callback killed its worker (os._exit, out of
            # memory): the pool is broken, and the crawl with it.
            raise _worker_died(name, request.url) from None
        for report in reports:
            _track.report(*report)
        for repair in repairs:
            self.ask_repair(*repair)
        try:
            for out in outputs:
                await self.handle(out)
            if error is not None:
                raise error[0]
        except Exception as e:
            self.counts["callback_errors"] += 1
            if self.settings.fail_fast:
                raise
            if error is not None and e is error[0] and error[1]:
                log.error("error in %s for %s (in a worker)\n%s", name, request.url, error[1])
            else:
                log.exception("error in %s for %s", name, request.url)

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
        if self.settings.max_items is not None and self.counts["items"] >= self.settings.max_items:
            raise _Stop("max_items")


#: Iterable, but one result rather than many.
_NOT_MANY = (dict, str, bytes, bytearray)
#: Never an item.
_NOT_ITEMS = (str, bytes, bytearray, int, float, bool, set, frozenset)


def _why(reason: str) -> str:
    return "robots.txt" if reason == "robots" else "TDM reserved"
