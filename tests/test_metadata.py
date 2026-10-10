import netweir
from netweir._native import _response

PAGE = """<html><head>
<meta property="og:title" content="Desk lamp">
<meta name="twitter:card" content="summary">
<script type="application/ld+json">
{"@context": "https://schema.org", "@type": "Product", "name": "Desk lamp",
 "offers": {"@type": "Offer", "price": "19.99", "priceCurrency": "USD"}}
</script>
</head><body>
<div itemscope itemtype="https://schema.org/Review">
  <span itemprop="author">Ada</span>
  <a itemprop="url" href="/reviews/1">Read</a>
</div>
</body></html>"""


def page(html, url="https://shop.example/lamp"):
    return netweir.Page(
        _response(url, 200, "HTTP/1.1", [("content-type", "text/html")], html.encode())
    )


def test_a_page_says_what_it_is_in_structured_data():
    meta = page(PAGE).metadata()
    product = meta["json_ld"][0]
    assert product["@type"] == "Product"
    assert product["offers"]["price"] == "19.99"
    assert meta["opengraph"] == {"og:title": "Desk lamp"}
    assert meta["twitter"] == {"twitter:card": "summary"}
    review = meta["microdata"][0]
    assert review["type"] == "https://schema.org/Review"
    assert review["properties"] == {"author": "Ada", "url": "https://shop.example/reviews/1"}


def test_a_page_without_structured_data_has_empty_sections():
    assert page("<p>hi</p>").metadata() == {
        "json_ld": [],
        "microdata": [],
        "opengraph": {},
        "twitter": {},
        "dublin_core": {},
    }


def test_parsed_html_has_it_too():
    meta = netweir.parse(PAGE).metadata("https://other.example/")
    assert meta["microdata"][0]["properties"]["url"] == "https://other.example/reviews/1"
