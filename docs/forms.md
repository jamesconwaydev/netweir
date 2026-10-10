# Forms, logins and APIs

Most of the web you scrape is GET requests, but not all of it. A search box
posts its query, a login posts a username and password, and a page's own
JavaScript often fetches its data from a JSON API with a POST. netweir sends
all of these, and sends each the way the browser in your profile would.

## Two kinds of POST

A browser sends a POST in one of two ways, and a server can tell them apart:

- **Submitting a form** is a navigation: the browser leaves the page and
  loads the answer. It sends `Origin`, a `Referer`, `Sec-Fetch-Mode:
  navigate` and, in Chrome, `Cache-Control: max-age=0`.
- **A script's request** (`fetch()` in the page's JavaScript) isn't. It
  sends `Accept: */*` and `Sec-Fetch-Mode: cors`, leaves out the headers a
  navigation carries, and orders the rest differently.

`form=` sends the first kind; `json=` and `body=` send the second. Chrome
154 and Firefox 156 were recorded doing both, and netweir's tests check its
requests match them byte for byte, header order included. Safari 27 hasn't
been recorded doing either yet, so a POST with `profile="safari"` raises
`FetchError` rather than guess.

## Sending data

```python
import netweir

# A form submission: URL-encoded, as a browser posts a form.
page = netweir.post("https://example.com/search", form={"q": "rust", "page": 2})

# A script's request with JSON, as a page's fetch() sends it.
page = netweir.post("https://example.com/api/search", json={"q": "rust"})

# Any body; without a Content-Type it's sent as text, as fetch() sends a string.
page = netweir.post("https://example.com/upload", body=b"a,b\n1,2", headers={"Content-Type": "text/csv"})

# Any method.
page = netweir.request("PUT", "https://example.com/api/items/7", json={"price": 9})
```

Pass one of `form=`, `json=` and `body=`. `referer=` names the page the
request comes from, which sets `Referer`, `Origin` and `Sec-Fetch-Site` as
the browser would; without it, a form or a script's request comes from the
root of the target's site. A GET with `referer=` looks like following a
link on that page:

```python
page = netweir.get("https://example.com/item/7", referer="https://example.com/list")
```

Redirects follow the browsers' rules. A 303, or a 301 or 302 after a POST,
turns into a GET without a body, which is how a login usually answers; a 307
or 308 repeats the request, body and all.

## Forms on a page

`page.form()` reads a form the way the browser would submit it: its method,
where it goes, how it's encoded, and the fields in order. That means the
named, enabled controls; checked checkboxes and radio buttons; each select's
selected options (or its first); textareas; and the submit button pressed.
Hidden fields come along, which is where sites keep their CSRF tokens.

```python
async with netweir.Client() as client:
    login = await client.get("https://example.com/login")
    form = login.form(data={"username": "ada", "password": "s3cret"})
    home = await client.submit(form)
```

Use a `Client` for anything with a session: it keeps the cookies the login
page set, and the form usually needs them. `netweir.submit(form)` works too,
but starts without any cookies.

Pick the form with a CSS selector or an XPath, or by `formid=`,
`formname=` or `formnumber=` (the first form otherwise). `data=` replaces
fields of the same name and adds new ones; a list value sends the name more
than once. `click=` names the submit button pressed; it's the first one
unless you say otherwise, and `click=False` presses none.

```python
form = page.form("form.search", data={"q": "maps", "sort": "price"}, click="go")
form = page.form(formname="lookup")
form.method, form.action, form.enctype, form.fields
```

A GET form puts its fields in the query string. A `multipart/form-data`
form is sent as multipart, with a boundary shaped as Chrome's; file uploads
aren't supported yet.

## In a crawl

`Request` takes the same arguments, and `Request.from_form(page, ...)`
submits a form on a page, as Scrapy's `FormRequest.from_response` does:

```python
class Search(netweir.Spider):
    start_urls = ["https://example.com/search"]

    def parse(self, page):
        for term in ["rust", "python"]:
            yield netweir.Request.from_form(page, data={"q": term}, callback=self.results)

    def results(self, page):
        for row in page.css(".result"):
            yield {"title": row.css("h2::text").get()}


class Api(netweir.Spider):
    def start(self):
        yield netweir.Request("https://example.com/api/items", method="POST", json={"page": 1})
```

`page.follow()` sends links as followed from the page they're on, with its
`Referer`.

The duplicate filter tells requests apart by their body as well as their
URL, so two searches for different terms are two requests. A checkpoint
keeps each request's method and body, and a resumed crawl sends them again
as they were.

Two things are different for a POST, because sending one twice can do
something twice (place an order, post a comment):

- A POST or PATCH that gets a server error (500, 502, 504 and the like), or
  loses its connection after it was sent, isn't retried. Pass
  `retry_post=True` on the request if repeating it is safe. A 429, or a 503
  with `Retry-After`, says the request wasn't processed, so it's retried as
  any other.
- It's never handed to Chrome. With `browser="on_block"`, a blocked POST
  goes to your `on_block` handler instead, and `browser=True` on a POST is a
  `ValueError`.
