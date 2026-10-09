"""Checkpoints: a crawl stopped at any point picks up where it was."""

import json
import random
import subprocess
import sys
import textwrap
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import pytest

import netweir

PAGES = 6
PER_PAGE = 5


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    hits: list[str] = []

    def log_message(self, *args):
        pass

    def do_GET(self):
        Handler.hits.append(self.path)
        p = self.path
        if p.startswith("/page/"):
            n = int(p.rsplit("/", 1)[1])
            links = "".join(f'<a class="item" href="/item/{n}-{i}">i</a>' for i in range(PER_PAGE))
            nxt = f'<a class="next" href="/page/{n + 1}">next</a>' if n < PAGES else ""
            body = f"<html><body>{links}{nxt}</body></html>"
        elif p.startswith("/item/"):
            body = f"<html><body><h1>{p.rsplit('/', 1)[1]}</h1></body></html>"
        else:
            body = "<html></html>"
        data = body.encode()
        # Slow enough that a kill lands mid-crawl.
        time.sleep(0.03)
        self.send_response(200)
        self.send_header("Content-Type", "text/html")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)


@pytest.fixture(scope="module")
def base():
    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    yield f"http://127.0.0.1:{server.server_port}"
    server.shutdown()


def settings(tmp_path, **kw):
    return netweir.Settings(
        throttle=False,
        start_delay=0,
        obey_robots=False,
        obey_tdmrep=False,
        checkpoint=str(tmp_path / "state"),
        **kw,
    )


class Stop(Exception):
    pass


def test_an_interrupted_crawl_resumes_without_losing_or_repeating(base, tmp_path):
    out = tmp_path / "items.jsonl"
    stop_after = [7]

    class Items(netweir.Spider):
        start_urls = [f"{base}/page/1"]

        def parse(self, page):
            for href in page.css("a.item::attr(href)").getall():
                yield page.follow(href, callback="item", meta={"from": page.url})
            if nxt := page.css("a.next::attr(href)").get():
                yield page.follow(nxt)

        def item(self, page):
            stop_after[0] -= 1
            if stop_after[0] < 0:
                raise Stop  # the crawl dies here, mid-page
            yield {"name": page.css("h1::text").get(), "from": page.meta["from"]}

    spider = Items()
    spider.settings = settings(tmp_path, fail_fast=True)
    with pytest.raises(Stop):
        spider.run(output=str(out))
    first = [json.loads(line) for line in out.read_text(encoding="utf-8").splitlines()]
    assert 0 < len(first) < PAGES * PER_PAGE

    stop_after[0] = 10**6
    spider = Items()
    spider.settings = settings(tmp_path)
    stats = spider.run(output=str(out))
    rows = [json.loads(line) for line in out.read_text(encoding="utf-8").splitlines()]
    names = sorted(r["name"] for r in rows)
    assert names == sorted(f"{n}-{i}" for n in range(1, PAGES + 1) for i in range(PER_PAGE))
    assert len({r["_id"] for r in rows}) == len(rows), "no item twice"
    assert all(r["from"].endswith(f"/page/{r['name'].split('-')[0]}") for r in rows)
    assert stats["fetched"] < PAGES + PAGES * PER_PAGE, "finished pages aren't fetched again"


def test_with_a_checkpoint_callbacks_must_be_methods(base, tmp_path):
    class Lambdas(netweir.Spider):
        start_urls = [f"{base}/page/1"]

        def parse(self, page):
            yield page.follow("/item/x", callback=lambda p: None)

    spider = Lambdas()
    spider.settings = settings(tmp_path)
    stats = spider.run()
    assert stats["callback_errors"] == 1


def test_meta_must_be_json(base, tmp_path, caplog):
    class Meta(netweir.Spider):
        start_urls = [f"{base}/page/1"]

        def parse(self, page):
            yield page.follow("/item/x", meta={"when": object()})

    spider = Meta()
    spider.settings = settings(tmp_path)
    with caplog.at_level("ERROR", logger="netweir"):
        assert spider.run()["callback_errors"] == 1
    assert "JSON" in caplog.text


SPIDER = textwrap.dedent(
    """
    import netweir

    class Items(netweir.Spider):
        start_urls = ["{base}/page/1"]

        def parse(self, page):
            for href in page.css("a.item::attr(href)").getall():
                yield page.follow(href, callback="item")
            if nxt := page.css("a.next::attr(href)").get():
                yield page.follow(nxt)

        def item(self, page):
            yield {{"name": page.css("h1::text").get()}}
    """
)


@pytest.mark.parametrize("output", ["items.jsonl", "items.csv"])
def test_kill_9_at_random_points_loses_nothing_and_repeats_nothing(base, tmp_path, output):
    script = tmp_path / "spider.py"
    script.write_text(SPIDER.format(base=base), encoding="utf-8")
    out = tmp_path / output
    command = [
        sys.executable, "-m", "netweir", "crawl", str(script), "-q", "-o", str(out),
        "-s", "throttle=false", "-s", "start_delay=0", "-s", "obey_robots=false",
        "-s", "obey_tdmrep=false", "-s", "per_domain=1", "-s", "concurrency=1",
        "-s", f"checkpoint={tmp_path / 'state'}",
    ]  # fmt: skip
    rng = random.Random(1234)
    kills = 0
    for _ in range(60):
        proc = subprocess.Popen(command, stderr=subprocess.PIPE, text=True)
        try:
            proc.wait(timeout=rng.uniform(0.15, 0.9))
        except subprocess.TimeoutExpired:
            proc.kill()  # SIGKILL: no cleanup of any kind
            proc.wait()
            kills += 1
            continue
        assert proc.returncode == 0, proc.stderr.read()
        break
    else:
        pytest.fail("the crawl never finished")
    assert kills > 0, "the test never interrupted the crawl"
    if output.endswith(".jsonl"):
        rows = [json.loads(line) for line in out.read_text(encoding="utf-8").splitlines()]
    else:
        import csv

        with out.open(encoding="utf-8", newline="") as f:
            rows = list(csv.DictReader(f))
    names = sorted(r["name"] for r in rows)
    expected = sorted(f"{n}-{i}" for n in range(1, PAGES + 1) for i in range(PER_PAGE))
    assert names == expected, f"after {kills} kills"
    assert len({r["_id"] for r in rows}) == len(rows)
