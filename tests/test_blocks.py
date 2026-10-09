"""Block pages: what a response means, and what netweir.get does about it."""

import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

import pytest

import netweir

FIXTURES = Path(__file__).parent.parent / "crates/netweir-core/tests/fixtures/blocks"


def fixture(name):
    raw = (FIXTURES / name).read_bytes()
    head, body = raw.split(b"\r\n\r\n", 1)
    lines = head.decode().split("\r\n")
    status = int(lines[0].split(" ")[1])
    headers = [tuple(line.split(": ", 1)) for line in lines[1:]]
    return status, headers, body


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        pass

    def do_GET(self):
        name = self.path.lstrip("/")
        status, headers, body = fixture(name)
        self.send_response(status)
        for k, v in headers:
            if k.lower() != "content-length":
                self.send_header(k, v)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


@pytest.fixture(scope="module")
def base():
    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    yield f"http://127.0.0.1:{server.server_port}"
    server.shutdown()


def test_get_raises_on_a_block(base):
    with pytest.raises(netweir.Blocked) as caught:
        netweir.get(f"{base}/cloudflare-block.http")
    assert caught.value.vendor == "cloudflare"
    assert caught.value.kind == "block"
    assert caught.value.page.status == 403
    assert "cloudflare" in str(caught.value)


def test_a_block_can_be_a_page(base):
    page = netweir.get(f"{base}/datadome-captcha.http", raise_on_block=False)
    assert page.outcome == "blocked"
    assert page.blocked == "datadome"


@pytest.mark.parametrize(
    "name, outcome",
    [
        ("ok-behind-cloudflare.http", "ok"),
        ("ok-mentions-vendors.http", "ok"),
        ("not-found.http", "http_error"),
        ("throttled.http", "throttled"),
        ("payment.http", "payment_required"),
    ],
)
def test_outcomes(base, name, outcome):
    page = netweir.get(f"{base}/{name}")
    assert page.outcome == outcome
    assert page.blocked is None


async def test_client_raises_too_and_get_many_can_collect(base):
    async with netweir.Client() as client:
        with pytest.raises(netweir.Blocked):
            await client.get(f"{base}/human-captcha.http")
        pages = await client.get_many(
            [f"{base}/ok-behind-cloudflare.http", f"{base}/akamai-block.http"],
            return_exceptions=True,
        )
    assert pages[0].outcome == "ok"
    assert isinstance(pages[1], netweir.Blocked) and pages[1].vendor == "akamai"
