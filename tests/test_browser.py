"""The browser driver from Python. Skipped without Chrome, unless
NETWEIR_REQUIRE_CHROME is set."""

import asyncio
import os
import socket
import subprocess
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import pytest

import netweir

PAGES = {
    "/": b"""<!DOCTYPE html><title>shop</title>
<ul id=list></ul>
<script>
  for (const name of ['tea', 'cake']) {
    document.querySelector('#list').insertAdjacentHTML('beforeend', `<li>${name}</li>`);
  }
</script>
<button id=more style="display:none" onclick="document.querySelector('#list').insertAdjacentHTML('beforeend', '<li>jam</li>')">more</button>
<script>setTimeout(() => document.querySelector('#more').style.display = '', 200)</script>
<a id=away href="/missing">away</a>
<form onsubmit="event.preventDefault(); window.searched = document.querySelector('#q').value">
  <input id=q>
</form>
""",
}


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def do_GET(self):
        if self.path == "/empty":
            self.send_response(204)
            self.end_headers()
            return
        body = PAGES.get(self.path)
        self.send_response(200 if body else 404)
        self.send_header("Content-Type", "text/html")
        if self.path == "/":
            self.send_header("Set-Cookie", "visit=1; Path=/")
        body = body or b"<p>missing</p>"
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


class Proxy(BaseHTTPRequestHandler):
    """Wants the login user:secret, and answers every request itself."""

    def log_message(self, *args):
        pass

    def do_GET(self):
        if self.headers.get("Proxy-Authorization") != "Basic dXNlcjpzZWNyZXQ=":
            self.send_response(407)
            self.send_header("Proxy-Authenticate", 'Basic realm="test"')
            self.send_header("Content-Length", "0")
            self.end_headers()
            return
        body = f"<p id=via>{self.path}</p>".encode()
        self.send_response(200)
        self.send_header("Content-Type", "text/html")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


@pytest.fixture(scope="module")
def base():
    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    yield f"http://127.0.0.1:{server.server_port}"
    server.shutdown()


@pytest.fixture
async def browser():
    try:
        b = await netweir.browser(timeout=30)
    except netweir.BrowserError as e:
        if "no Chrome found" in str(e) and not os.environ.get("NETWEIR_REQUIRE_CHROME"):
            pytest.skip(str(e))
        raise
    yield b
    await b.close()


async def test_a_rendered_page_reads_back_as_a_node(browser, base):
    page = await browser.new_page()
    response = await page.goto(base + "/")
    assert (response.status, response.url) == (200, base + "/")
    assert await page.title() == "shop"
    root = await page.parse()
    assert root.css("li::text").getall() == ["tea", "cake"]
    assert "<li>cake</li>" in await page.content()


async def test_actions_wait_and_evaluate_returns_python_values(browser, base):
    async with await browser.new_page() as page:
        await page.goto(base + "/")
        await page.click("#more")
        await page.wait_for("li:nth-child(3)")
        await page.fill("#q", "scones")
        await page.press("Enter")
        assert await page.evaluate("searched") == "scones"
        # Waiting starts before the click, so the navigation can't slip past.
        moved = asyncio.ensure_future(page.wait_for_navigation())
        await page.click("#away")
        moved = await moved
        assert (moved.status, moved.url) == (404, base + "/missing")
        assert await page.evaluate(
            "() => ({n: 1, f: 1.5, ok: true, none: null, list: [1, 'a']})"
        ) == {
            "n": 1,
            "f": 1.5,
            "ok": True,
            "none": None,
            "list": [1, "a"],
        }
    assert page.closed


async def test_failures_raise_what_they_are(browser, base):
    page = await browser.new_page()
    await page.goto(base + "/")
    with pytest.raises(netweir.BrowserTimeout, match="visible") as timed_out:
        await page.wait_for("#q-missing", timeout=0.5)
    assert isinstance(timed_out.value, TimeoutError)
    with pytest.raises(netweir.BrowserTimeout, match="nothing matched"):
        await page.click("#nothing-here", timeout=0.5)
    with pytest.raises(netweir.BrowserError, match="boom"):
        await page.evaluate("(() => { throw new Error('boom') })()")
    with pytest.raises(ValueError, match="wait must be"):
        await page.goto(base + "/", wait="soon")
    closed = socket.socket()
    closed.bind(("127.0.0.1", 0))
    port = closed.getsockname()[1]
    closed.close()
    with pytest.raises(netweir.FetchError, match="REFUSED") as refused:
        await page.goto(f"http://127.0.0.1:{port}/")
    assert refused.value.kind == "connect"
    # Chrome abandons a navigation to a 204; the server was reached.
    with pytest.raises(netweir.FetchError, match="ERR_ABORTED") as aborted:
        await page.goto(base + "/empty")
    assert aborted.value.kind == "other"
    for bad in (0, -1, float("inf"), 1e300):
        with pytest.raises(ValueError, match="timeout"):
            await page.click("#q", timeout=bad)


async def test_cookies_are_dicts_and_stay_in_their_context(browser, base):
    page = await browser.new_page()
    await page.goto(base + "/")
    cookies = await page.cookies()
    assert [(c["name"], c["value"], c["path"]) for c in cookies] == [("visit", "1", "/")]
    assert await (await browser.new_page()).cookies() == []

    async with await browser.new_context() as context:
        a, b = await context.new_page(), await context.new_page()
        await a.set_cookies([{"name": "k", "value": "v", "domain": "127.0.0.1"}])
        assert [(c["name"], c["value"], c["path"]) for c in await b.cookies()] == [("k", "v", "/")]
        with pytest.raises(ValueError, match="domain"):
            await a.set_cookies([{"name": "k", "value": "v"}])


async def test_screenshots_are_png_and_can_go_to_a_file(browser, base, tmp_path):
    page = await browser.new_page()
    await page.goto(base + "/")
    png = await page.screenshot(tmp_path / "shot.png", full_page=True)
    assert png.startswith(b"\x89PNG") and (tmp_path / "shot.png").read_bytes() == png


async def test_the_browser_can_be_awaited_or_used_as_a_context_manager():
    try:
        async with netweir.browser() as b:
            assert b.version.startswith("Chrome/")
            page = await b.new_page()
    except netweir.BrowserError as e:
        if "no Chrome found" in str(e) and not os.environ.get("NETWEIR_REQUIRE_CHROME"):
            pytest.skip(str(e))
        raise
    assert b.closed
    with pytest.raises(netweir.BrowserError):
        await page.goto("about:blank")
    with pytest.raises(netweir.BrowserError):
        await b.new_page()
    assert not any(
        m in b.methods_sent() for m in ("Runtime.enable", "Console.enable", "Log.enable")
    )


async def test_a_proxy_with_a_login_carries_the_pages():
    server = ThreadingHTTPServer(("127.0.0.1", 0), Proxy)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    try:
        try:
            b = await netweir.browser(proxy=f"http://user:secret@127.0.0.1:{server.server_port}")
        except netweir.BrowserError as e:
            if "no Chrome found" in str(e) and not os.environ.get("NETWEIR_REQUIRE_CHROME"):
                pytest.skip(str(e))
            raise
        async with b:
            page = await b.new_page()
            # Not a loopback address, which Chrome would fetch directly.
            response = await page.goto("http://shop.test/item")
            assert response.status == 200
            assert (await page.parse()).css("#via::text").get() == "http://shop.test/item"
    finally:
        server.shutdown()


async def test_a_running_chrome_can_be_driven_and_is_left_running(base, tmp_path):
    from netweir._native import find_chrome

    try:
        executable = find_chrome()
    except netweir.BrowserError as e:
        if not os.environ.get("NETWEIR_REQUIRE_CHROME"):
            pytest.skip(str(e))
        raise
    chrome = subprocess.Popen(
        [
            executable,
            "--headless",
            "--remote-debugging-port=0",
            "--no-first-run",
            f"--user-data-dir={tmp_path}",
            "about:blank",
        ],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.PIPE,
        text=True,
    )
    try:
        ws = next(
            line.split("DevTools listening on ", 1)[1].strip()
            for line in chrome.stderr
            if "DevTools listening on " in line
        )
        async with netweir.browser(connect=ws) as b:
            page = await b.new_page()
            await page.goto(base + "/")
            assert await page.title() == "shop"
        assert chrome.poll() is None, "Chrome kept running"
        with pytest.raises(ValueError, match="already running"):
            await netweir.browser(connect=ws, proxy="http://127.0.0.1:1")
        with pytest.raises(ValueError, match="already running"):
            await netweir.browser(connect=ws, headless=False)
    finally:
        chrome.kill()
        chrome.wait()
