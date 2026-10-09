"""The Beautiful Soup style API on Node: find, find_all and navigation."""

import re

import pytest

import netweir

PAGE = """<html><head><title>The Dormouse's story</title></head><body>
<p class="title"><b>The Dormouse's story</b></p>
<p class="story">Once upon a time there were three little sisters; and their names were
<a href="http://example.com/elsie" class="sister" id="link1">Elsie</a>,
<a href="http://example.com/lacie" class="sister" id="link2">Lacie</a> and
<a href="http://example.com/tillie" class="sister brother" id="link3">Tillie</a>;
and they lived at the bottom of a well.</p>
<p class="story">...</p>
<!-- the end -->
</body></html>"""


@pytest.fixture
def soup():
    return netweir.parse(PAGE)


def ids(nodes):
    return [n["id"] for n in nodes]


def test_find_all_by_name(soup):
    assert len(soup.find_all("a")) == 3
    assert [n.name for n in soup.find_all(["a", "b"])] == ["b", "a", "a", "a"]
    assert soup.find("title").text == "The Dormouse's story"
    assert soup.find("table") is None


def test_find_all_by_attributes(soup):
    assert ids(soup.find_all(id="link2")) == ["link2"]
    assert ids(soup.find_all(href=re.compile("elsie"))) == ["link1"]
    assert ids(soup.find_all("a", attrs={"id": ["link1", "link3"]})) == ["link1", "link3"]
    assert len(soup.find_all(id=True)) == 3
    assert [n.name for n in soup.find_all("p", class_=False)] == []
    assert len(soup.find_all(attrs={"class": "story"})) == 2


def test_class_matches_any_single_class_or_the_whole_value(soup):
    assert ids(soup.find_all("a", class_="sister")) == ["link1", "link2", "link3"]
    assert ids(soup.find_all("a", class_="brother")) == ["link3"]
    assert ids(soup.find_all("a", class_="sister brother")) == ["link3"]
    assert ids(soup.find_all(class_=re.compile("^bro"))) == ["link3"]


def test_find_all_with_functions(soup):
    def has_class_but_no_id(tag):
        return tag.get("class") is not None and tag.get("id") is None

    assert [n.name for n in soup.find_all(has_class_but_no_id)] == ["p", "p", "p"]
    assert ids(soup.find_all(id=lambda v: v is not None and v.endswith("3"))) == ["link3"]


def test_limit_and_recursive(soup):
    assert ids(soup.find_all("a", limit=2)) == ["link1", "link2"]
    body = soup.find("body")
    assert body.find_all("a", recursive=False) == []
    assert [n.name for n in body.find_all(recursive=False)] == ["p", "p", "p"]


def test_string_filter_finds_text(soup):
    elsie = soup.find(string="Elsie")
    assert str(elsie) == "Elsie"
    assert elsie.kind == "text"
    assert elsie.parent["id"] == "link1"
    assert [str(s) for s in soup.find_all(string=re.compile("story"))] == [
        "The Dormouse's story",
        "The Dormouse's story",
    ]
    assert soup.find("a", string="Lacie")["id"] == "link2"


def test_navigation(soup):
    link2 = soup.find(id="link2")
    assert link2.parent.name == "p"
    assert link2.find_previous_sibling("a")["id"] == "link1"
    assert link2.find_next_sibling("a")["id"] == "link3"
    assert ids(link2.find_next_siblings("a")) == ["link3"]
    assert str(link2.next_sibling) == " and\n"
    assert str(link2.previous_sibling) == ",\n"
    assert link2.find_parent("body").name == "body"
    assert [p.name for p in link2.find_parents()][:2] == ["p", "body"]
    assert str(link2.next_element) == "Lacie"
    assert link2.previous_element.kind == "text"
    assert link2.find_next("p").text == "..."
    assert soup.find("b").find_previous("title").text == "The Dormouse's story"
    assert [n.name for n in soup.find("b").find_all_next(["a", "p"])][:2] == ["p", "a"]
    assert soup.find_all("a")[2].find_all_previous("a")[0]["id"] == "link2"


def test_children_descendants_and_contents(soup):
    title = soup.find("p", class_="title")
    assert [n.name for n in title.children] == ["b"]
    assert title.contents == title.children
    assert [n.kind for n in title.descendants] == ["element", "text"]
    head = soup.find("head")
    assert [n.name for n in head.descendants if n.kind == "element"] == ["title"]


def test_string_and_get_text(soup):
    assert soup.find("b").string == "The Dormouse's story"
    assert soup.find("p", class_="title").string == "The Dormouse's story"
    assert soup.find("p", class_="story").string is None
    assert soup.find("head").get_text() == "The Dormouse's story"
    assert (
        soup.find("p", class_="story")
        .get_text("|", strip=True)
        .startswith(
            "Once upon a time there were three little sisters; and their names were|Elsie|,"
        )
    )


def test_attributes_like_beautiful_soup(soup):
    a = soup.find("a")
    assert a["href"] == "http://example.com/elsie"
    assert a.get("rel") is None
    assert a.get("rel", "none") == "none"
    with pytest.raises(KeyError):
        a["rel"]
    assert a.attrs == {"href": "http://example.com/elsie", "class": "sister", "id": "link1"}


def test_select_and_select_one(soup):
    assert ids(soup.select("p.story a.sister")) == ["link1", "link2", "link3"]
    assert soup.select_one("a#link2")["id"] == "link2"
    assert soup.select_one("table") is None


def test_comments_are_nodes(soup):
    comment = soup.find("body").contents[-2]
    assert comment.kind == "comment"
    assert comment.html == "<!-- the end -->"
    assert str(comment) == " the end ", "Beautiful Soup's str() of a comment is its text"


def test_str_of_a_node(soup):
    assert str(soup.find("b")) == "<b>The Dormouse's story</b>"
    assert repr(soup.find("b")) == "<Node b>"


def test_bad_filters_are_type_errors(soup):
    with pytest.raises(TypeError):
        soup.find_all(42)
    with pytest.raises(TypeError):
        soup.find_all(id=[1, 2])


def test_page_has_the_same_api():
    page_root = netweir.parse(PAGE)
    assert page_root.find("a")["id"] == "link1"


def test_text_leaves_out_scripts_and_styles_like_beautiful_soup():
    root = netweir.parse("<p>Hi <script>track()</script><style>p{}</style>there</p>")
    p = root.find("p")
    assert p.get_text() == "Hi there"
    assert p.text == "Hi there"
    assert root.find("style").get_text() == "p{}"
    assert p.xpath("string()").get() == "Hi track()p{}there"
