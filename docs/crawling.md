# Crawling

A spider is a class. netweir's engine, in Rust, runs the queue, robots.txt,
throttling and deduplication; your code says what to take from each page
and which links to follow.

## Spiders

```python
import netweir


class Quotes(netweir.Spider):
    start_urls = ["https://quotes.toscrape.com/"]
    settings = netweir.Settings(concurrency=32)

    async def parse(self, page):
        for quote in page.css("div.quote"):
            yield {
                "text": quote.css(".text::text").get(),
                "author": quote.css(".author::text").get(),
            }
        for request in page.follow_all(page.css("li.next a::attr(href)")):
            yield request
```

A callback receives a `Page` and yields (or returns) items and requests:

- An **item** is a dict or a dataclass instance.
- A **request** is `page.follow(href)`, which resolves `href` against the
  page (and its `<base href>`), or a `netweir.Request`.

A callback can be an async generator, a generator, a coroutine or a plain
function. Override `start()` instead of setting `start_urls` when the first
requests need more than a URL; it can be an async generator, a generator or
return a list.

`allowed_domains` keeps a crawl on the sites you mean it to visit:

```python
class Shop(netweir.Spider):
    start_urls = ["https://example.com/"]
    allowed_domains = ["example.com"]  # and www.example.com, shop.example.com...
```

A request to any other site is dropped, counted as `offsite` in the stats,
and logged once per site. A domain covers its subdomains, and one with a
port (`"example.com:8443"`) allows only that port. A request with
`dont_filter=True` goes anywhere. Put domains in the list, not URLs; a URL
is an error.

## Requests

```python
netweir.Request(
    "https://example.com/item/1",
    callback="item",            # a method name, or a function
    errback="lost",             # called with (request, error) if no response arrives
    meta={"category": "tools"}, # comes back as page.meta
    priority=5,                 # higher first, among one site's requests
    headers={"Accept-Language": "de-DE"},
    dont_filter=False,          # True fetches it even if seen before
    browser=False,              # True fetches it in Chrome
    method="GET",               # or POST, PUT...; with one of form=, json=, body=
    referer="https://example.com/",  # the page it comes from; page.follow sets it
)
```

`page.depth` is how many links the crawl followed from a start page to get
here. A request the same as one already seen (same method, URL and body,
ignoring fragments, query order and tracking parameters such as `utm_*`) is
dropped unless `dont_filter=True`. Forms, JSON and other bodies are in
[Forms, logins and APIs](forms.md).

A request with `browser=True` is loaded in Chrome, so the callback sees the
page after its JavaScript ran, and `page.browser` is the live page for
clicking and typing until the callback returns. See
[Browser](browser.md#in-a-crawl).

## Sitemaps

Most sites list their pages in sitemaps, which is often the quickest way to
reach every product or article. Point `sitemap_urls` at a sitemap, a
sitemap index, or the site's robots.txt (whose `Sitemap:` lines name them),
and say which callback each kind of page goes to:

```python
class Shop(netweir.Spider):
    sitemap_urls = ["https://example.com/robots.txt"]
    sitemap_rules = [
        ("/product/", "product"),  # the first regex a URL matches wins
        ("/blog/", "post"),
    ]
    sitemap_follow = ["/sitemap-products"]  # which sitemaps of an index to read

    def product(self, page):
        yield {"name": page.css("h1::text").get()}

    def post(self, page):
        ...
```

Pages no rule matches are skipped; with the default rules every page goes
to `parse`. `sitemap_follow` defaults to every sitemap an index lists.
Sitemaps can be XML (`urlset` or `sitemapindex`), gzipped (`.xml.gz`), or
plain text with one URL per line. `sitemap_alternate_links = True` also
crawls the other-language versions a sitemap gives for a page
(`xhtml:link rel="alternate"`).

To choose entries by what the sitemap says about them, override
`sitemap_filter`. Each entry is a dict with `loc`, `lastmod`,
`changefreq`, `priority` (None where the sitemap doesn't say) and
`alternates`:

```python
class Recent(netweir.Spider):
    sitemap_urls = ["https://example.com/sitemap.xml"]

    def sitemap_filter(self, entries):
        for entry in entries:
            if (entry["lastmod"] or "") >= "2026-10-01":
                yield entry
```

A sitemap that isn't valid, or ends part way through, is logged and
skipped. Sitemaps are read no further than `max_response_size`, after
unzipping, and XML entities a sitemap declares are never expanded, so a
hostile one can't fill memory.

`sitemap_urls` and `start_urls` can be used together.

## Running

```python
stats = Quotes().run(output="quotes.jsonl")
```

`run()` crawls to the end and returns the stats: pages fetched and failed,
items, duplicates, pages robots.txt kept out, retries, blocks and more.
`await spider.crawl(output=...)` does the same inside an event loop you
already have.

From the shell:

```
netweir crawl quotes.py -o quotes.jsonl -s concurrency=16 -s max_depth=3
```

`-s` sets any [setting](settings.md); `--spider NAME` picks one when the file
defines several; `-q` shows only warnings and errors. A mistake on the
command line (an unknown setting, a bad value, a path that can't be
written) exits with status 2 and one line saying why.

### Stopping early

A crawl that should end before it runs out of pages takes a limit:

```
netweir crawl quotes.py -o quotes.jsonl -s max_items=500 -s max_time=600
```

`max_items` stops it once that many items are written, exactly that many.
`max_pages` sends that many requests, a retry counting as one, and stops
once their pages have been through the callbacks. `max_errors` stops it
once callbacks and pipelines have raised that many exceptions, and
`max_time` after that many seconds; requests still in flight when one of
those three stops it are dropped, not waited for.
After a run, `spider.finish_reason` says why it ended: `"finished"` when
there was nothing left to fetch, or the name of the limit.

With a [checkpoint](self-healing.md#checkpoints), running a stopped crawl again carries on
from where it stopped, and the limits count afresh: `max_items=500` gives
the next 500.

## Callbacks in worker processes

The engine fetches and parses in Rust, but a spider's callbacks take turns
in one Python process. If yours do real work (heavy parsing, a model,
decoding images), they hold the whole crawl to one core. Spread them over
several:

```python
class Shop(netweir.Spider):
    settings = netweir.Settings(workers=4)
```

or `-s workers=4` from the shell. The engine, your pipelines, exporters
and the checkpoint stay in the main process; only callbacks move. You get
the same items, with the same ids, as with one process, and a checkpointed
crawl resumes the same way. `bench/workers.py` runs a spider whose
callbacks do 10 ms of work each: four workers finish it 3.1 times faster
than one, on a 12-core laptop.

What changes:

- Each worker builds its own spider from its class, so the class must be
  defined at module level, or in the file you gave `netweir crawl`. A
  script that runs a crawl with workers needs the usual
  `if __name__ == "__main__":` guard, as anything using multiprocessing
  does.
- Attributes a callback sets on the spider stay in that worker. To count
  or collect across pages, yield items and do it in a pipeline.
- Callbacks are passed between processes by name, so a request's callback
  should be a method of the spider (the same rule as a checkpoint). One
  that isn't runs in the main process instead; a request yielded in a
  worker must name a method.
- What travels between processes must pickle: items, `meta`, and the
  exception a callback raises. An item that can't go back ends that
  page's results there, counted as an error, as an exception would.
- An async callback runs on an event loop of its own in the worker, one
  per page.
- A spider defined in a notebook or the interactive prompt can't be
  rebuilt by a worker; put it in a file.
- Callbacks of browser requests run in the main process, where their live
  `page.browser` is.

Errors, warnings and tracked selectors work as they do in one process: a
callback's exception is logged and counted, and a relocated selector is
reported once per crawl. With `fail_fast`, the crawl stops as soon as the
error comes back from its worker, and callbacks still running are
stopped, not waited for. A worker that dies (out of memory, say) ends the
crawl with an error naming the page it was on; with a checkpoint, run it
again to carry on.

## Being polite

By default netweir reads each site's robots.txt (RFC 9309) and stays out of
what it disallows, skips pages whose owners reserve text and data mining
rights (TDMRep), and adapts its pace to each site: the delay between
requests moves toward the site's response time, and a 429 or a block slows
it further. Turning robots.txt off (`obey_robots=False`) logs a warning
once per crawl.

## Pipelines

Every item passes through the spider's pipelines, in order:

```python
def in_stock(item):
    return item if item["stock"] > 0 else None   # None drops it


async def with_currency(item):
    return {**item, "currency": "GBP"}


class Books(netweir.Spider):
    pipelines = [in_stock, with_currency, netweir.export.jsonl("books.jsonl")]
```

A stage is any function, sync or async, that returns the item, a changed
item, or `None` to drop it. An exception in a callback or a pipeline is
logged with the page's URL and counted, and the crawl carries on; set
`fail_fast=True` to stop at the first one.

## Exporters

`run(output=...)`, `-o` and the exporters write items in Rust:

| | File | Notes |
|---|---|---|
| `netweir.export.jsonl(path)` | `.jsonl`, `.jl` | one JSON object per line, keys in the item's order |
| `netweir.export.csv(path, fields=None)` | `.csv` | columns are `fields`, or the first item's keys; others are left out with one warning |
| `netweir.export.parquet(path)` | `.parquet` | column types from the first 1,000 items; values that don't fit are stored as null, with a warning |

Dates become ISO 8601 strings, `Decimal`s and integers too big for 64 bits
become strings, and NaN becomes null. Each run writes the file afresh,
except a run that [resumes from a checkpoint](self-healing.md#checkpoints),
which adds to it.
