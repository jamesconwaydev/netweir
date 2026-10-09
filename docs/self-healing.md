# Self-healing

Long crawls meet three kinds of trouble: sites that block them, crashes,
and redesigns. netweir deals with each.

## Blocks

Every response is classified before your code sees it:

| `page.outcome` | Means |
|---|---|
| `ok` | a page |
| `blocked` | a bot-protection page; `page.blocked` names the vendor |
| `throttled` | a 429, or a 503 with Retry-After |
| `payment_required` | a 402; its `crawler-price` header is reported |
| `http_error` | any other 4xx or 5xx |

Block pages are recognised for Cloudflare, Akamai, DataDome, HUMAN, Kasada,
Imperva and AWS WAF, from headers first, then cookies, then the page.
Outside a crawl, `netweir.get()` raises `netweir.Blocked` (with `vendor`,
`kind` and `page`); pass `raise_on_block=False` to get the page instead.

In a crawl, a request that fails climbs a ladder:

1. Server errors (408, 500, 502, 503, 504, 522, 524) and network errors are
   retried after a random wait that grows each time (`backoff_base`,
   `backoff_max`), up to `retries` times. Other 4xx and 402 aren't retried.
2. A block replaces the site's session: an empty cookie jar, and the next of
   `proxies` if you gave more than one.
3. Blocks and 429s slow the site down; Retry-After is honoured.
4. A site that blocked more than `breaker_ratio` of its last
   `breaker_window` responses is paused for `breaker_pause` seconds, with
   one warning. It judges after 10 responses, so a site that blocks from
   the start pauses quickly.
5. With `browser="on_block"`, a request still blocked after its retries is
   loaded once in Chrome, which has time to pass a challenge. If it gets
   through, the callback gets the page, and the cookies Chrome earned go to
   the site's HTTP session, so the next requests don't need Chrome.
6. A request still blocked after all that goes to your spider's
   `on_block(request, page)`, which logs it by default and may yield items
   or requests like a callback.

The stats count `retries`, `blocked`, `throttled`, `sessions_replaced`,
`breaker_trips`, `browser_fetches` and `browser_unblocked`.

## Checkpoints

```python
class Books(netweir.Spider):
    settings = netweir.Settings(checkpoint="crawls/books")
```

or `-s checkpoint=crawls/books` on the command line. The crawl's state lives
in `crawls/books/crawl.sqlite3`. Run the same crawl again after it stops,
however it stopped, and it resumes:

- requests it hadn't finished are fetched; ones it had aren't;
- pages it had seen stay seen;
- items already written aren't written again. Each carries an `_id` that is
  the same on every run (as a key in dict items; dataclass items are
  deduplicated by it without showing it).

JSONL and CSV output is exact across a crash: a resumed run cuts the file
back to the last point recorded and adds to it, so every item is in the file
once. A Parquet file is readable only once closed: it's written as
`books.parquet.partial` and renamed when its items are on record, and a
resumed run writes the next part beside it (`books.1.parquet`). A crawl
that can't start leaves its output files as they were.

With a checkpoint, callbacks are saved by name, so they must be methods of
the spider, and `meta` must be JSON. Delete the directory to start the crawl
over.

## Tracked selectors

```python
price = page.css(".price_color::text", track="price")
price.relocated   # True when the selector missed and the element was found by similarity
price.score       # 1.0 for a match, the similarity when relocated
```

Each time the selector matches, netweir saves what the element looks like:
its tag, text, attributes, the elements around it and the text just before
it. When a redesign breaks the selector, every element of the same kind is
scored against that, and the best one is used if it scores at least
`track_threshold` (0.75). A warning says so, once per site and name in each
crawl. Below the threshold, nothing is returned, and `score` says how close
the nearest candidate came.

Tracking is careful not to invent data:

- It learns, as the crawl goes, whether an element's text and the label
  before it change from page to page (a product's price and name do), and
  doesn't hold those differences against a candidate.
- A label that never changed and now reads differently rules a candidate
  out: if the Total row is gone, the Shipping cell isn't used instead.
- If several elements still look exactly like the remembered one (the
  prices in a list), the selector missed one item, not a redesign, and
  nothing is relocated.

So track elements that should always be there. A tracked query is one
selector that finds an element, optionally with an ending such as
`::text`.

`track=` also works on `xpath()` and on Item fields. The stats count
`relocated` and `lost`. Fingerprints are kept per site in the checkpoint
file, or in `~/.netweir/tracks.db` (`$NETWEIR_HOME/tracks.db`) otherwise.

## Repair proposals

```python
class Books(netweir.Spider):
    repair = netweir.repair.llm(report="repairs.jsonl")
```

When a tracked selector breaks, the repair plugin sends the old selector,
what the element looked like and the page to a language model, and asks for
a new selector. Each proposal is checked on the page (does it parse, what
does it find, how similar is that to the old element), logged, kept on
`spider.repairs` and appended to the report. **Nothing is applied**; you
decide.

By default it uses Anthropic's API (`pip install anthropic`, with
`ANTHROPIC_API_KEY` set). Pass `complete=` any function that takes a prompt
and returns text to use another model.

The model is sent the page's HTML without scripts, styles and comments (up
to 40,000 characters), its URL, the old selector and what the element looked
like. If pages hold things you'd rather not send, such as form tokens or
personal data, use a `complete` that redacts them or calls a local model.

## Traps

`max_depth` stops following links that far from a start page,
`max_pages_per_domain` caps the requests for one site, and a URL whose path
repeats the same run of segments three times in a row (`/a/b/a/b/a/b`), or
one segment four times (`/a/a/a/a`), is refused.
