# netweir

A web scraping library for Python, with the heavy lifting done in Rust.

This is early. What works today is the parser: hand it a page, ask for what
you want with CSS, get strings back.

```python
import netweir

page = netweir.parse(html)

page.css("h1::text").get()  # "All products"
page.css(".price_color::text").getall()  # ["£51.77", "£53.74", ...]
page.css("article h3 a::attr(href)").getall()  # ["/catalogue/...", ...]

for book in page.css("article.product_pod"):
    print(book.css("h3 a::attr(title)").get(), book.css(".price_color::text").get())
```

If you've used Scrapy, `::text`, `::attr()`, `get()` and `getall()` mean what
you think they mean.

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

Pages you don't control can be built to be slow to parse. Pass a timeout and
netweir gives up instead of hanging:

```python
netweir.parse(html, timeout=2.0)  # raises netweir.ParseTimeout
```

## Building

You need Rust, a C compiler and [uv](https://docs.astral.sh/uv/).

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
