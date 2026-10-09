"""XPath and Beautiful Soup style search, timed as a scraper uses them.

    uv run --group bench python bench/query.py
    uv run --group bench python bench/query.py --check   # exit 1 if netweir loses

Each library parses the generated shop page, then answers the same
questions. Timing covers parse and queries together, because that is what a
scraper pays per page; netweir's document index, built on the first query,
is inside the time. The figure is the best of N runs, in milliseconds.
"""

import argparse
import re
import sys
import time

from pages import page

import netweir

XPATHS = [
    "//article[contains(concat(' ', normalize-space(@class), ' '), ' product_pod ')]"
    "//p[@class='price_color']/text()",
    "//h3/a/@title",
    "//article//a[1]/@href",
    "//p[contains(@class, 'star-rating')]/@class",
    "count(//article[.//p[contains(., 'In stock')]])",
    "//ol/li[last()]//h3/a/text()",
]


def xpath_netweir(html):
    root = netweir.parse(html)
    return [root.xpath(q).getall() for q in XPATHS]


def xpath_lxml(html):
    from lxml import html as lxml_html

    tree = lxml_html.document_fromstring(html)
    out = []
    for q in XPATHS:
        r = tree.xpath(q)
        if isinstance(r, float):
            out.append([repr(r)])
        else:
            out.append([str(x) for x in r])
    return out


def xpath_parsel(html):
    import parsel

    sel = parsel.Selector(text=html)
    return [sel.xpath(q).getall() for q in XPATHS]


PRICE = re.compile(r"£")


def soup_queries(root):
    return [
        [a["href"] for a in root.find_all("a", title=True)],
        [p.get_text() for p in root.find_all("p", class_="price_color")],
        [p.get_text(strip=True) for p in root.find_all("p", class_="instock")],
        len(root.find_all(string=PRICE)),
        [img["alt"] for img in root.find_all("img", limit=50)],
        root.find("ul", class_="pager").find("a")["href"],
    ]


def soup_netweir(html):
    return soup_queries(netweir.parse(html))


def soup_bs4(html):
    from bs4 import BeautifulSoup

    return soup_queries(BeautifulSoup(html, "lxml"))


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


def compare(title, runners, html, runs):
    expected = runners["netweir"](html)
    times = {}
    for name, fn in runners.items():
        got = fn(html)
        if got != expected:
            sys.exit(f"{title}: {name} gave different answers from netweir")
        times[name] = best_ms(fn, html, runs)
    print(f"  {title}")
    for name, ms in sorted(times.items(), key=lambda kv: kv[1]):
        print(f"    {name:14} {ms:9.2f} ms  {ms / times['netweir']:5.2f}x")
    return times


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--check", action="store_true", help="fail if netweir is clearly slower")
    ap.add_argument("--tolerance", type=float, default=0.05)
    ap.add_argument("--runs", type=int, default=15)
    args = ap.parse_args()

    losses = []
    # lxml's time on some of these queries grows with the square of the
    # page, so 3 MB rather than 10 keeps the run to minutes.
    for label, size in [("1 MB", 1_000_000), ("3 MB", 3_000_000)]:
        html = page(size)
        print(f"\n{label} page", flush=True)
        xp = compare(
            "XPath",
            {"netweir": xpath_netweir, "lxml": xpath_lxml, "parsel": xpath_parsel},
            html,
            args.runs,
        )
        sp = compare(
            "find_all", {"netweir": soup_netweir, "beautifulsoup": soup_bs4}, html, args.runs
        )
        for kind, times in (("XPath", xp), ("find_all", sp)):
            rival = min(v for k, v in times.items() if k != "netweir")
            if times["netweir"] > rival * (1 + args.tolerance):
                losses.append(f"{kind} on the {label} page")

    if args.check and losses:
        sys.exit(f"netweir is slower than the fastest alternative: {', '.join(losses)}")


if __name__ == "__main__":
    main()
