"""A declarative spider against the same spider written with callbacks.

    uv run python bench/rules.py

A local shop of 50 listing pages and 1,000 product pages is served from a
separate process, so the server doesn't share this process's CPU or GIL.
Both spiders follow the pagination, visit every product and extract the
same four fields; throttling and robots.txt are off, because the point is
what the crawler costs, not the network. The figures are the best of three
runs: wall time and this process's CPU time.

The declarative spider can use as much CPU or more: the parsing is the
same, it just happens on other threads, several pages at once, instead of
waiting its turn for Python. What it saves is wall time and the GIL.
"""

import multiprocessing
import random
import resource
import sys
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

from pages import product

import netweir

LISTINGS = 50
PER_PAGE = 20


def listing(n: int) -> str:
    rng = random.Random(n)
    items = "".join(product(rng, (n - 1) * PER_PAGE + i) for i in range(PER_PAGE))
    nxt = f'<li class="next"><a href="/page/{n + 1}">next</a></li>' if n < LISTINGS else ""
    return f"<html><body><ol>{items}</ol><ul class='pager'>{nxt}</ul></body></html>"


def detail(i: int) -> str:
    rng = random.Random(i)
    rows = "".join(f"<tr><th>Row {k}</th><td>{rng.random()}</td></tr>" for k in range(8))
    return (
        f"<html><body><div class='product_main'><h1>Item {i}</h1>"
        f"<p class='price_color'>£{rng.randint(5, 60)}.{rng.randint(10, 99)}</p>"
        f"<p class='availability'>In stock ({rng.randint(1, 30)} available)</p></div>"
        f"<table><tr><th>UPC</th><td>upc{i:06d}</td></tr>{rows}</table>"
        f"<article>{'<p>' + 'Lorem ipsum dolor sit amet. ' * 40 + '</p>'}</article>"
        "</body></html>"
    )


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        pass

    def do_GET(self):
        p = self.path
        if p.startswith("/page/"):
            body = listing(int(p.rsplit("/", 1)[1]))
        elif p.startswith("/catalogue/item-"):
            body = detail(int(p.split("item-")[1].split("/")[0]))
        else:
            self.send_response(404)
            self.send_header("Content-Length", "0")
            self.end_headers()
            return
        data = body.encode()
        self.send_response(200)
        self.send_header("Content-Type", "text/html; charset=utf-8")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)


def serve(port):
    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    port.put(server.server_port)
    server.serve_forever()


SETTINGS = netweir.Settings(
    throttle=False,
    start_delay=0,
    obey_robots=False,
    obey_tdmrep=False,
    per_domain=16,
)


class Book(netweir.Item):
    title = netweir.css("h1::text")
    price = netweir.css(".price_color::text", re=r"[\d.]+", into=float)
    stock = netweir.css(".availability::text", re=r"(\d+) available", into=int)
    upc = netweir.xpath("//th[.='UPC']/following-sibling::td/text()")


class Declarative(netweir.Spider):
    settings = SETTINGS
    rules = [
        netweir.Follow("li.next a"),
        netweir.Follow("article.product_pod h3 a", extract=Book),
    ]


class Callbacks(netweir.Spider):
    settings = SETTINGS

    async def parse(self, page):
        for href in page.css("article.product_pod h3 a::attr(href)").getall():
            yield page.follow(href, callback=self.book)
        if nxt := page.css("li.next a::attr(href)").get():
            yield page.follow(nxt)

    async def book(self, page):
        price = page.css(".price_color::text").re_first(r"[\d.]+")
        stock = page.css(".availability::text").re_first(r"(\d+) available")
        yield {
            "title": page.css("h1::text").get(),
            "price": float(price),
            "stock": int(stock),
            "upc": page.xpath("//th[.='UPC']/following-sibling::td/text()").get(),
        }


def cpu() -> float:
    r = resource.getrusage(resource.RUSAGE_SELF)
    return r.ru_utime + r.ru_stime


def measure(spider_class, base):
    best = None
    for _ in range(3):
        items = []
        spider = spider_class()
        spider.start_urls = [f"{base}/page/1"]
        spider.pipelines = [lambda item, items=items: items.append(item) or item]
        c0, t0 = cpu(), time.perf_counter()
        spider.run()
        took = (time.perf_counter() - t0, cpu() - c0)
        if len(items) != LISTINGS * PER_PAGE:
            sys.exit(f"{spider_class.__name__} got {len(items)} items")
        best = took if best is None or took[0] < best[0] else best
    return best, sorted(items, key=lambda i: i["upc"])


def main():
    port = multiprocessing.Queue()
    server = multiprocessing.Process(target=serve, args=(port,), daemon=True)
    server.start()
    base = f"http://127.0.0.1:{port.get()}"
    try:
        (d_wall, d_cpu), d_items = measure(Declarative, base)
        (c_wall, c_cpu), c_items = measure(Callbacks, base)
    finally:
        server.terminate()
    if d_items != c_items:
        sys.exit("the two spiders extracted different data")
    print(f"{LISTINGS + LISTINGS * PER_PAGE} pages, {len(d_items)} items")
    print(f"  declarative  {d_wall * 1000:7.0f} ms wall  {d_cpu * 1000:7.0f} ms CPU")
    print(
        f"  callbacks    {c_wall * 1000:7.0f} ms wall  {c_cpu * 1000:7.0f} ms CPU"
        f"  ({c_wall / d_wall:.1f}x the wall time)"
    )


if __name__ == "__main__":
    main()
