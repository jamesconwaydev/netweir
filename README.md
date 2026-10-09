# netweir

An ultra-fast, self-healing, undetectable web scraper. Rust does the work;
you write Python.

That's the finished product, and it isn't finished yet. Here's what each
of those words will mean when it is:

- **Ultra-fast.** Fetching, parsing, crawl scheduling and extraction all
  run in Rust, off Python's GIL. A spider that only declares what it wants
  never touches Python per page. Every speed claim is a benchmark in this
  repository that you can run.
- **Self-healing.** Selectors that find their element again after a site
  redesign and tell you they had to. Blocks spotted and recovered from by
  rotating, backing off or slowing down. Crawls that pick up where they
  stopped after a crash.
- **Undetectable.** Requests that are byte-for-byte what a real browser
  sends, from the TLS handshake to the order of the headers, and later a
  real browser for the pages that need one.

## Where it is now

Working today:

- Fetching that is indistinguishable from Chrome 154 on the wire, over
  HTTP/2 and HTTP/1.1, with cookies and redirects. Below: how that's
  proved, and the one known exception.
- A parser that beats selectolax, the one to beat in Python, on the same
  pages.
- CSS queries with Scrapy's `::text` and `::attr()`, all of XPath 1.0, and
  Beautiful Soup's `find_all` family, each faster than the library you'd
  otherwise use for it.

Coming next, in order: crawling with spiders, robots.txt and throttling; declarative spiders that run entirely
in Rust; self-healing selectors, block recovery and crash-safe resume; then
a browser driver. The design is in [docs/design/v0.1.md](docs/design/v0.1.md).

## Quick look

```python
import netweir

page = netweir.get("https://books.toscrape.com/")

page.css("h1::text").get()  # "All products"
page.css(".price_color::text").getall()  # ["£51.77", "£53.74", ...]
page.css("article h3 a::attr(href)").getall()  # ["/catalogue/...", ...]

for book in page.css("article.product_pod"):
    print(book.css("h3 a::attr(title)").get(), book.css(".price_color::text").get())
```

If you've used Scrapy, `::text`, `::attr()`, `get()` and `getall()` mean what
you think they mean. Already have the HTML? `netweir.parse(html)` skips the
request and gives you something you query the same way.

## Bring your selectors with you

Whatever you scrape with now, your selectors should work here unchanged.
XPath is all of XPath 1.0, plus the extras parsel users lean on:

```python
page.xpath("//article//h3/a/@title").getall()
page.xpath("//p[has-class('price_color')]/text()").get()
page.xpath("//article//a[re:test(@href, '_\\d+/index\\.html$')]/@href").getall()
page.xpath("//li[@class=$cls]", cls="next").get()  # variables, as in parsel
page.css(".price_color::text").re(r"[\d.]+")  # ["51.77", "53.74", ...]
```

And if you think in Beautiful Soup, the same page answers that way too:

```python
import re

page.find("ul", class_="pager").find("a")["href"]
page.find_all("a", title=True, limit=10)
page.find_all(string=re.compile("£"))
page.find("h3").find_parent("li").find_next_sibling("li")
```

Two things work slightly differently, on purpose. `get()` on an element
gives its HTML the way Chrome's `outerHTML` writes it, so an attribute
holding a quote comes out as `&quot;` where lxml would switch to single
quotes; the elements and their order are the same. And attribute values are
plain strings: `node["class"]` is `"star-rating Three"`, not Beautiful
Soup's list. Filters still match one class out of several, as you'd expect.

## Looking like a browser

Most sites don't block scrapers by reading their code. They block them by
the first few hundred bytes of the connection: which TLS ciphers and
extensions arrive, what the HTTP/2 settings frame says, which headers come
first. A Python HTTP library gives itself away before it has
asked for anything.

netweir sends what Chrome 154 sends. Not something close to it: the same
ClientHello, the same HTTP/2 settings and priorities, the same headers in
the same order and the same capitalisation over HTTP/1.1. Cookies go where
Chrome puts them, and redirects are followed hop by hop the way Chrome
follows them, header order after a redirect included.

You don't have to take that on trust. Chrome was recorded doing four
navigations against a local server: a page that sets a cookie, a revisit,
a redirect to another site and one within the site. `cargo test -p
netweir-core` has netweir do the same four and fails if any request differs
from Chrome's. The public checker at tls.peet.ws reports the same JA4
fingerprint for both: `t13d1517h2_8daaf6152771_cb7bf5808d99`.

One known difference: Chrome sends a `priority` header only over HTTP/2,
and netweir can't tell a server lacks HTTP/2 until it has answered once. So
the first request to an https server that only speaks HTTP/1.1 carries that
header; every request after it doesn't.

For many pages at once, use a client. It keeps connections and cookies
between requests, and the requests run concurrently in Rust:

```python
async with netweir.Client(proxy="http://user:pass@proxy:8080") as client:
    pages = await client.get_many(urls)
```

A request that fails before any response arrives raises `netweir.FetchError`,
and its `kind` says why: `"timeout"`, `"connect"`, `"tls"` and so on. A 404 is
still a page; check `page.status`.

## Why it's fast

Pages are parsed by [lexbor](https://github.com/lexbor/lexbor), the same
spec-compliant C parser selectolax uses. The difference is what happens
after: netweir runs the whole query in Rust and hands Python a finished list
of strings, instead of a Python object for every element along the way. The
GIL is released while it works, so threads parse in parallel.

`bench/parse.py` pulls every price, title and link out of a generated shop
page:

| | 1 MB page | 10 MB page |
|---|---|---|
| netweir | 5.0 ms | 61 ms |
| selectolax | 5.3 ms | 64 ms |
| BeautifulSoup (lxml) | 196 ms | 2.3 s |
| lxml + cssselect | 347 ms | 82 s |
| parsel | 353 ms | 82 s |

Measured on an Apple M-series laptop with Python 3.14. The lead over selectolax is small because both use lexbor; it comes from doing the extraction in Rust. Run it yourself:
`uv run --group bench python bench/parse.py`.

XPath and `find_all` are a different story, because the libraries people
use for them are much slower. `bench/query.py` parses the same shop page and
asks six XPath questions, or six Beautiful Soup ones:

| | 1 MB page | 3 MB page |
|---|---|---|
| netweir, XPath | 9.9 ms | 33 ms |
| lxml, XPath | 809 ms | 8.8 s |
| parsel, XPath | 814 ms | 8.8 s |
| netweir, `find_all` | 9.4 ms | 29 ms |
| Beautiful Soup, `find_all` | 170 ms | 515 ms |

Look at lxml across the row: three times the page took eleven times as long.
netweir builds a flat index of the document on the first query, so `//x` is
a scan over a few arrays, and its time grows with the page and no faster.
Both benchmarks run in CI, and the build fails if any library beats netweir.

Pages you don't control can be built to be slow to parse. Pass a timeout and
netweir gives up instead of hanging:

```python
netweir.parse(html, timeout=2.0)  # raises netweir.ParseTimeout
```

The timeout bounds time, not memory. A small page built to abuse the HTML5
spec's rules for misnested formatting tags can still make any spec-compliant
parser allocate gigabytes; a cap on that comes with the crawler.

## Building

You need Rust, a C compiler, CMake and [uv](https://docs.astral.sh/uv/).

```
git clone --recurse-submodules https://github.com/jamesconwaydev/netweir
cd netweir
uv sync --group dev
uv run maturin develop --uv
uv run pytest
```

## Licence

AGPL-3.0. If that doesn't work for your company, a commercial licence is
available. See NOTICE for the licences of what netweir bundles.

Please use it the way you'd want your own site scraped. netweir obeys
robots.txt once crawling lands; until then, that's on you.
