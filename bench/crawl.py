"""A whole crawl, timed: netweir against Scrapy, Scrapling and a plain
httpx + selectolax script.

    uv run python bench/crawl.py                     # every crawler it can import
    uv run python bench/crawl.py --only netweir      # just one

A local server plays 100 sites (site0.localhost ... site99.localhost) of 100
pages each, every answer 50 ms late, the way a real server keeps you
waiting. Each crawler starts at every site's front page, follows every link
and pulls a title and a price from each of the 10,000 pages, with the same
limits: 100 requests in flight, 8 per site, no robots.txt, no throttling.
Each runs in its own process, so its CPU time and peak memory are its own.

Install what you want to compare against (Scrapling needs Python 3.13 or
older for now):

    uv pip install scrapy "scrapling[fetchers]" httpx selectolax
"""

from __future__ import annotations

import argparse
import asyncio
import json
import os
import random
import subprocess
import sys
import tempfile
import time

SITES = 100
PAGES = 100
LATENCY = 0.05
CONCURRENCY = 100
PER_SITE = 8


# --- the sites ---------------------------------------------------------------


def page(site: int, n: int) -> bytes:
    rng = random.Random(site * 1000 + n)
    links = "".join(
        f'<li><a class="next" href="/p/{(n * 3 + k) % PAGES}">more {k}</a></li>' for k in (1, 2, 3)
    )
    filler = "".join(
        f"<article class='card'><h3>{rng.random():.6f}</h3><p>{'lorem ipsum ' * 8}</p></article>"
        for _ in range(12)
    )
    return (
        f"<!doctype html><html><head><title>Site {site}</title></head><body>"
        f"<h1 class='title'>Site {site} page {n}</h1><p class='price'>£{rng.randint(1, 99)}.99</p>"
        f"<ul class='nav'>{links}</ul>{filler}</body></html>"
    ).encode()


async def handle(reader: asyncio.StreamReader, writer: asyncio.StreamWriter) -> None:
    try:
        while True:
            head = await reader.readuntil(b"\r\n\r\n")
            lines = head.decode("latin-1").split("\r\n")
            path = lines[0].split(" ")[1]
            host = next(
                (
                    line.split(":", 1)[1].strip()
                    for line in lines[1:]
                    if line.lower().startswith("host:")
                ),
                "",
            )
            await asyncio.sleep(LATENCY)
            try:
                site = int(host.split(".")[0].removeprefix("site"))
                n = int(path.rsplit("/", 1)[1]) if path.startswith("/p/") else 0
                body, status = page(site, n), b"200 OK"
            except ValueError:
                body, status = b"not found", b"404 Not Found"
            writer.write(
                b"HTTP/1.1 " + status + b"\r\nContent-Type: text/html; charset=utf-8\r\n"
                b"Content-Length: " + str(len(body)).encode() + b"\r\n\r\n" + body
            )
            await writer.drain()
    except (asyncio.IncompleteReadError, ConnectionError, IndexError):
        pass
    finally:
        writer.close()


async def serve(port_file: str) -> None:
    # *.localhost resolves to ::1 or 127.0.0.1 depending on the client, so
    # listen on both, on one port.
    v6 = await asyncio.start_server(handle, host="::1", port=0, backlog=1024)
    port = v6.sockets[0].getsockname()[1]
    v4 = await asyncio.start_server(handle, host="127.0.0.1", port=port, backlog=1024)
    with open(port_file, "w") as f:
        f.write(str(port))
    async with v6, v4:
        await asyncio.gather(v6.serve_forever(), v4.serve_forever())


def start_urls(port: int, path: str = "p/0") -> list[str]:
    """Every site's first page. Page 0 is also linked from page 33, so a
    crawler sees each of the 10,000 pages once."""
    return [f"http://site{s}.localhost:{port}/{path}" for s in range(SITES)]


# --- the crawlers --------------------------------------------------------------
# Each prints the number of items it extracted.


def run_netweir(port: int, declarative: bool) -> int:
    import netweir

    settings = netweir.Settings(
        concurrency=CONCURRENCY,
        per_domain=PER_SITE,
        throttle=False,
        start_delay=0,
        obey_robots=False,
        obey_tdmrep=False,
        retries=0,
    )
    items = []

    if declarative:

        class Page(netweir.Item):
            title = netweir.css("h1.title::text")
            price = netweir.css("p.price::text")

        class Crawl(netweir.Spider):
            rules = [netweir.Follow("a.next", extract=Page, follow=True)]

        # Rules extract from the pages they lead to, not from start pages: start
        # at a front page that /p/0 is linked from (the same content).
        Crawl.start_urls = start_urls(port, path="")
    else:

        class Crawl(netweir.Spider):
            def parse(self, page):
                yield {
                    "title": page.css("h1.title::text").get(),
                    "price": page.css("p.price::text").get(),
                }
                for href in page.css("a.next::attr(href)").getall():
                    yield page.follow(href)

        Crawl.start_urls = start_urls(port)
    Crawl.settings = settings
    Crawl.pipelines = [lambda item: items.append(item) or item]
    logging_off()
    Crawl().run()
    return len(items)


def run_scrapy(port: int) -> int:
    import scrapy
    from scrapy.crawler import CrawlerProcess

    count = [0]

    class Crawl(scrapy.Spider):
        name = "bench"

        async def start(self):
            for url in start_urls(port):
                yield scrapy.Request(url)

        def parse(self, response):
            count[0] += 1
            yield {
                "title": response.css("h1.title::text").get(),
                "price": response.css("p.price::text").get(),
            }
            for href in response.css("a.next::attr(href)").getall():
                yield response.follow(href)

    process = CrawlerProcess(
        settings={
            "CONCURRENT_REQUESTS": CONCURRENCY,
            "CONCURRENT_REQUESTS_PER_DOMAIN": PER_SITE,
            "ROBOTSTXT_OBEY": False,
            "AUTOTHROTTLE_ENABLED": False,
            "DOWNLOAD_DELAY": 0,
            "RETRY_ENABLED": False,
            "LOG_LEVEL": "ERROR",
            "TELNETCONSOLE_ENABLED": False,
        }
    )
    process.crawl(Crawl)
    process.start()
    return count[0]


def run_scrapling(port: int) -> int:
    from scrapling.spiders import Request, Spider

    count = [0]

    class Crawl(Spider):
        name = "bench"
        start_urls = start_urls(port)
        concurrent_requests = CONCURRENCY
        concurrent_requests_per_domain = PER_SITE
        robots_txt_obey = False
        autothrottle_enabled = False
        download_delay = 0
        logging_level = 40

        async def parse(self, response):
            count[0] += 1
            yield {
                "title": response.css("h1.title::text").get(),
                "price": response.css("p.price::text").get(),
            }
            for href in response.css("a.next::attr(href)").getall():
                yield Request(response.urljoin(href), callback=self.parse)

    Crawl().start()
    return count[0]


def run_httpx(port: int) -> int:
    import httpx
    from selectolax.lexbor import LexborHTMLParser

    async def crawl() -> int:
        seen = set(start_urls(port))
        queue: asyncio.Queue[str] = asyncio.Queue()
        for url in seen:
            queue.put_nowait(url)
        per_site: dict[str, asyncio.Semaphore] = {}
        count = 0
        limits = httpx.Limits(max_connections=CONCURRENCY, max_keepalive_connections=CONCURRENCY)
        async with httpx.AsyncClient(limits=limits, timeout=30) as client:

            async def worker() -> None:
                nonlocal count
                while True:
                    url = await queue.get()
                    host = httpx.URL(url).host
                    gate = per_site.setdefault(host, asyncio.Semaphore(PER_SITE))
                    async with gate:
                        response = await client.get(url)
                    tree = LexborHTMLParser(response.text)
                    title = tree.css_first("h1.title")
                    price = tree.css_first("p.price")
                    _ = (title and title.text(), price and price.text())
                    count += 1
                    for a in tree.css("a.next"):
                        link = str(response.url.join(a.attributes["href"]))
                        if link not in seen:
                            seen.add(link)
                            queue.put_nowait(link)
                    queue.task_done()

            workers = [asyncio.create_task(worker()) for _ in range(CONCURRENCY)]
            await queue.join()
            for w in workers:
                w.cancel()
        return count

    return asyncio.run(crawl())


def logging_off() -> None:
    import logging

    logging.getLogger("netweir").setLevel(logging.ERROR)


CRAWLERS = {
    "netweir": lambda port: run_netweir(port, declarative=False),
    "netweir-declarative": lambda port: run_netweir(port, declarative=True),
    "scrapy": run_scrapy,
    "scrapling": run_scrapling,
    "httpx+selectolax": run_httpx,
}
NEEDS = {
    "netweir": "netweir",
    "netweir-declarative": "netweir",
    "scrapy": "scrapy",
    "scrapling": "scrapling.spiders",
    "httpx+selectolax": "httpx",
}


# --- running and measuring ------------------------------------------------------


def measure(name: str, port: int) -> dict:
    started = time.perf_counter()
    proc = subprocess.Popen(
        [sys.executable, __file__, "--crawler", name, "--port", str(port)],
        stdout=subprocess.PIPE,
        text=True,
    )
    _, status, usage = os.wait4(proc.pid, 0)
    wall = time.perf_counter() - started
    out = proc.stdout.read().strip().splitlines()
    if status != 0 or not out:
        return {"name": name, "error": f"exit status {status}"}
    items = int(out[-1])
    # ru_maxrss is bytes on macOS, kilobytes on Linux.
    rss = usage.ru_maxrss / (1 if sys.platform == "darwin" else 1 / 1024) / 1e6
    cpu = usage.ru_utime + usage.ru_stime
    return {
        "name": name,
        "items": items,
        "pages_per_sec": items / wall,
        "cpu_ms_per_page": cpu * 1000 / max(items, 1),
        "peak_rss_mb": rss,
        "wall_s": wall,
    }


def available(name: str) -> bool:
    try:
        __import__(NEEDS[name])
    except Exception:  # noqa: BLE001 - anything that stops the import
        return False
    return True


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--crawler", help=argparse.SUPPRESS)
    ap.add_argument("--port", type=int, help=argparse.SUPPRESS)
    ap.add_argument("--serve", help=argparse.SUPPRESS)
    ap.add_argument("--only", action="append", choices=list(CRAWLERS))
    ap.add_argument("--json", action="store_true", help="print the results as JSON")
    args = ap.parse_args()
    if args.crawler:
        print(CRAWLERS[args.crawler](args.port))
        return
    if args.serve:
        asyncio.run(serve(args.serve))
        return

    port_file = os.path.join(tempfile.mkdtemp(prefix="netweir-bench-"), "port")
    server = subprocess.Popen([sys.executable, __file__, "--serve", port_file])
    try:
        for _ in range(100):
            if os.path.exists(port_file):
                break
            time.sleep(0.05)
        with open(port_file) as f:
            port = int(f.read())
        names = args.only or list(CRAWLERS)
        results = []
        for name in names:
            if not available(name):
                print(f"  {name:20} not installed, skipped", flush=True)
                continue
            result = measure(name, port)
            results.append(result)
            if "error" in result:
                print(f"  {name:20} failed: {result['error']}", flush=True)
                continue
            print(
                f"  {name:20} {result['pages_per_sec']:7.0f} pages/s  "
                f"{result['cpu_ms_per_page']:6.2f} ms CPU/page  "
                f"{result['peak_rss_mb']:6.0f} MB peak  ({result['items']} items)",
                flush=True,
            )
        if args.json:
            print(json.dumps(results, indent=2))
    finally:
        server.terminate()
        if os.path.exists(port_file):
            os.remove(port_file)


if __name__ == "__main__":
    main()
