# Browser

Some pages are built by JavaScript after they load, and some sites won't
show anything until a real browser has passed their checks. For those,
netweir drives Chrome. It's an async API:

```python
import netweir

async with netweir.browser() as browser:
    page = await browser.new_page()
    response = await page.goto("https://quotes.toscrape.com/js/")
    await page.click("li.next a")
    await page.wait_for("div.quote")
    root = await page.parse()
    quotes = root.css("span.text::text").getall()
```

`page.parse()` gives a netweir `Node`, so everything in
[Selecting](selecting.md) works on the rendered page.

## Starting Chrome

`netweir.browser()` starts an installed Chrome: the one at
`$NETWEIR_CHROME`, or else the first of Google Chrome, Chrome for Testing
and Chromium in their usual places. Its options:

| Option | Default | Does |
|---|---|---|
| `executable` | `None` | the Chrome to start, instead of searching |
| `headless` | `True` | `False` opens a window |
| `args` | `()` | more Chrome command-line switches |
| `timeout` | `30.0` | the default limit, in seconds, for navigations and actions |

Use it with `async with`, or `await` it and call `await browser.close()`
yourself. Each browser gets a fresh profile, deleted when it closes.

## Pages and contexts

`browser.new_page()` opens a page with cookies and storage of its own, so
two pages never see each other's logins. For pages that should share them,
open a context:

```python
async with await browser.new_context() as context:
    first = await context.new_page()
    second = await context.new_page()
```

Closing a context closes its pages; closing the browser closes everything.

## Navigating

`await page.goto(url, wait="load")` loads the page and waits for its
`load` event; `wait="domcontentloaded"` returns sooner, and
`wait="networkidle"` waits until nothing has been requested for half a
second. It returns a `BrowserResponse` with the `status`, final `url` and
`headers` of the document. A 404 is returned like any page; a page that
couldn't be reached at all raises `netweir.FetchError`, as `netweir.get()`
does.

`page.url` is where the page is now, `await page.title()` its title and
`await page.content()` its HTML as Chrome currently has it.

## Acting on the page

| Method | Does |
|---|---|
| `click(selector)` | clicks the middle of the element with the mouse |
| `fill(selector, text)` | replaces what's in a field with `text`, as typing would |
| `press(key)` | presses a key: a character, or `"Enter"`, `"Tab"`, `"Escape"`, `"Backspace"`, `"Delete"`, the arrows, `"Home"`, `"End"`, `"PageUp"`, `"PageDown"` |
| `wait_for(selector, state="visible")` | waits until the element is `"attached"`, `"visible"`, `"hidden"` or `"detached"` |

Selectors are CSS. You don't need to wait before acting: `click` and
`fill` keep trying until the element can take the action, up to the
timeout. A click waits for the element to exist, be visible, stop moving,
be enabled and not be covered by anything else; `fill` waits for a
visible, enabled field that isn't read-only.

If the time runs out, `netweir.BrowserTimeout` (a `TimeoutError`) says
which of those never happened, such as "something else was always at its
centre, covering it". Each method takes `timeout=` in seconds.

## Running JavaScript

```python
count = await page.evaluate("document.querySelectorAll('li').length")
data = await page.evaluate("() => fetch('/api/items').then(r => r.json())")
```

`evaluate` runs an expression, or a function, which it calls. A promise
is awaited, and the result comes back as Python values (dicts, lists,
strings, numbers, `True`, `False`, `None`). An error thrown in the page
raises `netweir.BrowserError` with its message.

## Screenshots and cookies

`await page.screenshot("page.png", full_page=True)` saves a PNG and returns
its bytes; leave out the path to get only the bytes.

`await page.cookies()` returns the cookies of the page's context as dicts
with `name`, `value`, `domain`, `path`, `expires`, `http_only`, `secure`
and `same_site`. `await page.set_cookies([...])` takes the same dicts;
`name`, `value` and `domain` are required.

## Not looking automated

A browser that's being driven usually shows it, and bot protection looks.
netweir avoids the giveaways:

- `navigator.webdriver` is false.
- Headless Chrome calls itself `HeadlessChrome` in its user agent. netweir
  sends the normal user agent instead, with the client hints Chrome
  reports for itself, in the request headers, in the page and in workers.
- It never turns on the DevTools domains whose side effects page scripts
  can notice, and its own scripts run where the page can't see them.
- Clicks and keys are real input events, not JavaScript calls.

A test in the repository loads a page that checks each of these from page
script, and fails if any of them shows.

Service workers still see the `HeadlessChrome` user agent. Pass
`headless=False` where that matters.
