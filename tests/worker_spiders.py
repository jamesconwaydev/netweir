"""Spiders for the worker tests, in a module of their own so that worker
processes can import them."""

import os
import time

import netweir

BASE = os.environ.get("NETWEIR_TEST_BASE", "")


class Shop(netweir.Spider):
    start_urls: list[str] = []

    def parse(self, page):
        for href in page.css("a.item::attr(href)").getall():
            yield page.follow(href, callback="item", meta={"from": page.url})

    def item(self, page):
        # Enough work that the pages spread over the workers.
        time.sleep(0.05)
        yield {
            "name": page.css("h1::text").get(),
            "from": page.meta["from"],
            "depth": page.depth,
        }
        yield {"pid": os.getpid()}


class Breaks(netweir.Spider):
    start_urls: list[str] = []

    def parse(self, page):
        yield {"before": page.url}
        raise ValueError(f"broke on {page.url}")


class Tracks(netweir.Spider):
    start_urls: list[str] = []

    def parse(self, page):
        yield {"price": page.css(".price_color::text", track="price").get()}
