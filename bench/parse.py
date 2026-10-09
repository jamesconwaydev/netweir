"""Parse + query speed against the Python parsers people use today.

    uv run --group bench python bench/parse.py
    uv run --group bench python bench/parse.py --check   # exit 1 if selectolax wins

Each library parses the page and pulls out every product's price, title and
link, which is the work a scraper does on every page. The figure is the best
of N runs, in milliseconds.
"""

import argparse
import sys
import time

from pages import page

import netweir

QUERIES = {
    "price": "article.product_pod .price_color::text",
    "title": "article.product_pod h3 a::attr(title)",
    "link": "article.product_pod h3 a::attr(href)",
}
CSS = {k: v.split("::")[0] for k, v in QUERIES.items()}


def run_netweir(html):
    root = netweir.parse(html)
    return [root.css(q).getall() for q in QUERIES.values()]


def run_selectolax(html):
    from selectolax.lexbor import LexborHTMLParser

    tree = LexborHTMLParser(html)
    return [
        [n.text(deep=False) for n in tree.css(CSS["price"])],
        [n.attributes["title"] for n in tree.css(CSS["title"])],
        [n.attributes["href"] for n in tree.css(CSS["link"])],
    ]


def run_lxml(html):
    from lxml import html as lxml_html

    tree = lxml_html.fromstring(html)
    return [
        [e.text for e in tree.cssselect(CSS["price"])],
        [e.get("title") for e in tree.cssselect(CSS["title"])],
        [e.get("href") for e in tree.cssselect(CSS["link"])],
    ]


def run_parsel(html):
    import parsel

    sel = parsel.Selector(html)
    return [sel.css(q).getall() for q in QUERIES.values()]


def run_bs4(html):
    from bs4 import BeautifulSoup

    soup = BeautifulSoup(html, "lxml")
    return [
        [e.get_text() for e in soup.select(CSS["price"])],
        [e["title"] for e in soup.select(CSS["title"])],
        [e["href"] for e in soup.select(CSS["link"])],
    ]


RUNNERS = {
    "netweir": run_netweir,
    "selectolax": run_selectolax,
    "lxml": run_lxml,
    "parsel": run_parsel,
    "beautifulsoup": run_bs4,
}


def best_ms(fn, html, runs, budget=3.0):
    """Best of `runs` timings, stopping early once `budget` seconds are spent."""
    best = float("inf")
    spent = 0.0
    for _ in range(runs):
        start = time.perf_counter()
        fn(html)
        took = time.perf_counter() - start
        best = min(best, took)
        spent += took
        if spent > budget:
            break
    return best * 1000


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--check", action="store_true", help="fail if netweir is not fastest")
    ap.add_argument("--runs", type=int, default=15)
    args = ap.parse_args()

    expected = None
    slower = []
    for label, size in [("1 MB", 1_000_000), ("10 MB", 10_000_000)]:
        html = page(size)
        expected = run_netweir(html)
        print(f"\n{label} page, {len(expected[0])} products", flush=True)
        times = {}
        for name, fn in RUNNERS.items():
            if fn(html) != expected:
                sys.exit(f"{name} extracted different data from netweir on the {label} page")
            times[name] = best_ms(fn, html, args.runs)
        for name, ms in sorted(times.items(), key=lambda kv: kv[1]):
            print(f"  {name:14} {ms:9.2f} ms  {ms / times['netweir']:5.2f}x")
        if times["selectolax"] < times["netweir"]:
            slower.append(label)

    if args.check and slower:
        sys.exit(f"netweir is slower than selectolax on: {', '.join(slower)}")


if __name__ == "__main__":
    main()
