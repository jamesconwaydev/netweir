"""A spider with heavy callbacks, in one process and in worker processes.

    uv run python bench/workers.py

The same local shop as bench/rules.py (50 listing pages, 1,000 products),
served from its own process. The callback for each product does about
10 ms of pure-Python work, standing in for parsing that's more than a few
selectors, a model, or decoding. In one process, those callbacks queue
for the GIL however fast the engine fetches; with workers, they run side
by side. The figure is wall time for the whole crawl.
"""

import hashlib
import multiprocessing
import os
import sys
import time

import netweir

sys.path.insert(0, os.path.dirname(__file__))
from rules import serve  # noqa: E402

SETTINGS = dict(
    throttle=False,
    start_delay=0,
    obey_robots=False,
    obey_tdmrep=False,
    per_domain=16,
)


class Heavy(netweir.Spider):
    def parse(self, page):
        for href in page.css("article.product_pod h3 a::attr(href)").getall():
            yield page.follow(href, callback="book")
        if nxt := page.css("li.next a::attr(href)").get():
            yield page.follow(nxt)

    def book(self, page):
        text = page.css("article p::text").get().encode()
        digest = text
        # About 10 ms of work that holds the GIL.
        for _ in range(4000):
            digest = hashlib.sha256(digest + text).digest()
        yield {"title": page.css("h1::text").get(), "digest": digest.hex()[:12]}


def measure(base: str, workers: int) -> tuple[float, int]:
    items = []
    spider = Heavy()
    spider.start_urls = [f"{base}/page/1"]
    spider.settings = netweir.Settings(workers=workers, **SETTINGS)
    spider.pipelines = [lambda item: items.append(item) or item]
    started = time.perf_counter()
    spider.run()
    return time.perf_counter() - started, len(items)


def main() -> None:
    port = multiprocessing.Queue()
    server = multiprocessing.Process(target=serve, args=(port,), daemon=True)
    server.start()
    base = f"http://127.0.0.1:{port.get()}"
    print(f"{os.cpu_count()} cores")
    baseline = None
    for workers in (1, 2, 4):
        took, count = measure(base, workers)
        baseline = baseline or took
        print(f"  workers={workers}  {took:6.2f} s  {baseline / took:4.1f}x  ({count} items)")
    server.terminate()


if __name__ == "__main__":
    main()
