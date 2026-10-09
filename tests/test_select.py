"""Scrapy-style selection: css(), xpath(), and what a Selection gives back."""

import re
import threading

import pytest

import netweir

PAGE = """<html><body>
<div id="main" class="content wide">
  <h1>All products</h1>
  <article class="product"><h3><a href="/a" title="Book A">A</a></h3><p class="price">£1.00</p></article>
  <article class="product"><h3><a href="/b" title="Book B">B</a></h3><p class="price">£2.50</p></article>
  <article class="product sold"><h3><a href="/c">C</a></h3><p class="price"><span>£</span>3.75</p></article>
</div>
<ul class="pager"><li class="next"><a href="page-2.html">next</a></li></ul>
</body></html>"""


@pytest.fixture
def page():
    return netweir.parse(PAGE)


def test_get_on_elements_is_outer_html_like_parsel(page):
    assert page.css("h1").get() == "<h1>All products</h1>"
    assert page.css("li.next").getall() == ['<li class="next"><a href="page-2.html">next</a></li>']
    assert page.xpath("//h1").get() == "<h1>All products</h1>"


def test_xpath_text_and_attributes(page):
    assert page.xpath("//h1/text()").get() == "All products"
    assert page.xpath("//article/h3/a/@href").getall() == ["/a", "/b", "/c"]
    assert page.xpath("//p[@class='price']/text()").getall() == ["£1.00", "£2.50", "3.75"]
    assert page.xpath("normalize-space(//article[3]/p)").get() == "£3.75"


def test_xpath_scalars_format_like_parsel(page):
    assert page.xpath("count(//article)").get() == "3.0"
    assert page.xpath("boolean(//article)").get() == "1"
    assert page.xpath("string(//h1)").get() == "All products"


def test_xpath_relative_to_a_node(page):
    second = page.css("article")[1]
    assert second.xpath("h3/a/text()").get() == "B"
    assert second.xpath(".//@title").get() == "Book B"
    assert page.css("article").xpath("h3/a/@href").getall() == ["/a", "/b", "/c"]


def test_xpath_variables_like_parsel(page):
    assert page.xpath("//a[@href=$h]/text()", h="/c").get() == "C"
    assert page.xpath("//article[$n]/h3/a/text()", n=2).get() == "B"
    with pytest.raises(netweir.XPathError, match=r"\$missing"):
        page.xpath("//a[@href=$missing]")


def test_xpath_extensions(page):
    assert page.xpath("//div[has-class('wide')]/@id").get() == "main"
    assert page.xpath("//a[re:test(@href, '^/[ab]$')]/text()").getall() == ["A", "B"]


def test_bad_xpath_raises(page):
    with pytest.raises(netweir.XPathError):
        page.xpath("//li[")
    with pytest.raises(ValueError):  # XPathError is a ValueError
        page.xpath("count(1)")


def test_comma_lists_keep_each_ending_in_document_order(page):
    assert page.css("li.next a::attr(href), h1::text").getall() == ["All products", "page-2.html"]
    assert page.css("h1, h1::text").getall() == ["<h1>All products</h1>", "All products"]


def test_items_are_nodes_or_strings(page):
    first = page.css("article a")[0]
    assert isinstance(first, netweir.Node)
    assert page.css("article a::text")[0] == "A"
    assert page.css("article a::attr(href)")[-1] == "/c"
    assert list(page.css("h3 a::text")) == ["A", "B", "C"]


def test_slices_are_selections(page):
    tail = page.css("article")[1:]
    assert isinstance(tail, netweir.Selection)
    assert tail.css("a::text").getall() == ["B", "C"]
    assert page.css("article a::text")[::2].getall() == ["A", "C"]
    assert page.css("article")[5:].getall() == []


def test_re_and_re_first_like_parsel(page):
    prices = page.css(".price::text")
    assert prices.re(r"[\d.]+") == ["1.00", "2.50", "3.75"]
    assert prices.re(r"£(\d+)\.(\d+)") == ["1", "00", "2", "50"]
    assert prices.re_first(r"\d+\.\d+") == "1.00"
    assert prices.re_first(r"zzz") is None
    assert prices.re_first(r"zzz", default="none") == "none"
    assert prices.re(re.compile(r"£\d")) == ["£1", "£2"]


def test_attrib_and_legacy_names(page):
    links = page.css("article a")
    assert links.attrib == {"href": "/a", "title": "Book A"}
    assert page.css("table").attrib == {}
    assert links.extract() == links.getall()
    assert links.extract_first() == links.get()


def test_css_on_strings_is_a_type_error(page):
    with pytest.raises(TypeError, match="text"):
        page.css("a::attr(href)").css("b")
    with pytest.raises(TypeError):
        page.css("h1::text").css("b")


def test_nodes_compare_and_hash_by_identity_in_the_document(page):
    a = page.css("h1")[0]
    b = page.xpath("//h1")[0]
    assert a == b
    assert hash(a) == hash(b)
    assert len({a, b, page.css("article")[0]}) == 2
    assert a != netweir.parse(PAGE).css("h1")[0], "same markup, different document"


def test_threads_share_xpath_and_css(page):
    errors = []

    def work():
        try:
            for _ in range(50):
                assert len(page.xpath("//article")) == 3
                assert page.css("a::attr(href)").getall()[-1] == "page-2.html"
        except Exception as e:  # noqa: BLE001 - report any failure from the thread
            errors.append(e)

    threads = [threading.Thread(target=work) for _ in range(8)]
    for t in threads:
        t.start()
    for t in threads:
        t.join()
    assert errors == []


def test_threads_build_a_fresh_documents_indexes_together():
    for _ in range(20):
        doc = netweir.parse(PAGE)
        start = threading.Barrier(8)
        results = []

        def work(doc=doc, start=start, results=results):
            start.wait()
            results.append((len(doc.xpath("//article")), doc.xpath("id('main')/h1/text()").get()))

        threads = [threading.Thread(target=work) for _ in range(8)]
        for t in threads:
            t.start()
        for t in threads:
            t.join()
        assert results == [(3, "All products")] * 8


def test_re_honours_compiled_flags_like_parsel():
    doc = netweir.parse("<pre>line1\nline2</pre><p>A1 b2</p>")
    pre = doc.css("pre::text")
    assert pre.re(re.compile(r"line1.line2", re.S)) == ["line1\nline2"]
    assert pre.re(re.compile(r"^line\d$", re.M)) == ["line1", "line2"]
    assert pre.re(re.compile(r"line \d  # a number", re.X)) == ["line1", "line2"]
    assert doc.css("p::text").re(re.compile(r"[a-z]\d", re.I)) == ["A1", "b2"]
    # Python-only syntax works, because Python's re does the matching.
    assert doc.css("p::text").re(r"(?<=A)\d") == ["1"]


def test_re_extract_group_and_entities_like_parsel():
    doc = netweir.parse("<p>1.0 2.5 3.7</p><a title='say \"hi\" &amp; &lt;go&gt;'>x</a>")
    assert doc.css("p::text").re(r"(?P<extract>\d)\.(\d)") == ["1"]
    assert doc.css("p::text").re(r"(\d)\.(\d)") == ["1", "0", "2", "5", "3", "7"]
    html = doc.css("a").re(r"title=\"(.*?)\">")
    assert html == ['say "hi" &amp; &lt;go>']
    assert doc.css("a").re(r"title=\"(.*?)\">", replace_entities=False) == [
        "say &quot;hi&quot; &amp; &lt;go&gt;"
    ]


def test_defaults_can_be_anything_like_parsel(page):
    assert page.css("nothing::text").get(default=0) == 0
    assert page.css("nothing::text").re_first(r"\d", default=0) == 0
    assert page.css("nothing").extract_first(default=[]) == []
