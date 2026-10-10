# Selecting

A page, and every element in it, answers CSS, XPath and Beautiful Soup
style queries. They return the same kinds of things, so you can mix them.

```python
import netweir

doc = netweir.parse("""
<ul class="books">
  <li class="book"><a href="/a" title="Book A">A</a><span class="price">£1.00</span></li>
  <li class="book sold"><a href="/b" title="Book B">B</a><span class="price">£2.50</span></li>
</ul>
""")
```

`netweir.parse(html)` gives you a document without fetching anything;
`netweir.get(url)` gives you a page you query the same way.

## CSS

```python
doc.css("li.book a::attr(href)").getall()    # ['/a', '/b']
doc.css(".price::text").get()                # '£1.00'
doc.css("li.sold").get()                     # '<li class="book sold">...</li>'
doc.css("a::attr(title), .price::text").getall()
# ['Book A', '£1.00', 'Book B', '£2.50']: each selector keeps its ending,
# and results come back in document order
```

- `::text` gives an element's own text nodes; ` ::text` (with a space)
  gives every text node below it.
- `::attr(name)` gives an attribute's value.
- With neither, `get()` gives the element's HTML, the way Chrome's
  `outerHTML` writes it.

## XPath

All of XPath 1.0, plus the functions Scrapy users rely on:

```python
doc.xpath("//li[span[contains(., '2.50')]]/a/@title").get()   # 'Book B'
doc.xpath("count(//li)").get()                                # '2.0'
doc.xpath("//li[has-class('sold')]/a/text()").get()           # 'B'
doc.xpath("//a[re:test(@href, '^/[ab]$')]/@href").getall()     # ['/a', '/b']
doc.xpath("//li[a/@title = $t]/span/text()", t="Book A").get() # '£1.00'
```

Keyword arguments to `xpath()` bind `$variables`. A query that can't be
parsed raises `netweir.XPathError`; a CSS selector that can't be parsed
raises `netweir.SelectorError`.

## Selections

`css()` and `xpath()` return a `Selection`, a list of results:

| | Gives |
|---|---|
| `get(default=None)` | the first result as a string, or `default` |
| `getall()` | every result as a string |
| `re(pattern)` | every match of a regular expression in the results (the groups if it has any) |
| `re_first(pattern, default=None)` | the first of those |
| `attrib` | the first element's attributes, as a dict |
| `sel[0]`, `sel[1:3]`, `len(sel)` | indexing and slicing |
| `sel.css(...)`, `sel.xpath(...)` | a query run below every element in it |

Indexing gives a `Node` for an element and a `str` for text or an
attribute's value. `extract()` and `extract_first()` are the older names
for `getall()` and `get()`.

## Beautiful Soup style

The same document answers Beautiful Soup's methods:

```python
import re

doc.find("a")["href"]                           # '/a'
[a["title"] for a in doc.find_all("a")]         # ['Book A', 'Book B']
doc.find("li", class_="sold").find("a").string  # 'B'
doc.find_all(string=re.compile("£"))            # the two prices
doc.find("span").find_parent("li")["class"]     # 'book'
```

`find`, `find_all` and the directional families (`find_parent(s)`,
`find_next_sibling(s)`, `find_previous_sibling(s)`, `find_next`,
`find_all_next`, `find_previous`, `find_all_previous`) take the filters you
know: a string, a list, `True`/`False`, a compiled regex or a function, with
`class_=`, `string=` (or `text=`), `limit=` and `recursive=`. Navigation
(`parent`, `children`, `contents`, `descendants`, siblings,
`next_element`), `get_text()`, `.string`, `select()` and `select_one()`
work as in Beautiful Soup.

Two differences, both on purpose:

- Attribute values are strings. `node["class"]` is `"book sold"`, not a
  list; filters still match one class out of several.
- `.text` and `get_text()` leave out the contents of `<script>`, `<style>`
  and `<template>`, as Beautiful Soup 4.10 and later do.

## Following an element through a redesign

Give a query a name with `track=`, and netweir remembers what its element
looked like. When a redesign breaks the query, netweir finds the most
similar element instead. See [Self-healing](self-healing.md#tracked-selectors).

## Structured data

Many pages describe themselves for search engines and social networks:
a product's name, price and rating in JSON-LD, an article's title and image
in Open Graph tags, reviews marked up as microdata. When it's there, it's
usually the cleanest copy of the data on the page, and `page.metadata()`
reads all of it at once:

```python
meta = page.metadata()

for thing in meta["json_ld"]:  # every <script type="application/ld+json">
    if thing.get("@type") == "Product":
        price = thing["offers"]["price"]

meta["microdata"]   # [{"type": "https://schema.org/Review", "properties": {...}}]
meta["opengraph"]   # {"og:title": "...", "og:image": [...], "product:price:amount": "..."}
meta["twitter"]     # {"twitter:card": "summary_large_image", ...}
meta["dublin_core"] # {"dc.title": "...", ...}
```

JSON-LD comes as written, with a block that's a list giving its items one
by one; a block that isn't valid JSON is skipped. Microdata follows the
HTML standard: nested items, `itemref`, several names in one `itemprop`,
and each element's value where the standard says to find it (`content`,
`href` and `src` made absolute against the page, `datetime`, `value`, or
the text). In the meta tag sections and in microdata, a key given more than
once has a list of its values. A parsed document has it too:
`netweir.parse(html).metadata(base_url)`.

