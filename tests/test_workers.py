"""Callbacks in worker processes: Settings(workers=N)."""

import logging
import os
import pickle
import subprocess
import sys
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import pytest
import worker_spiders

import netweir


def test_a_response_survives_pickling():
    from netweir._native import _response

    r = _response("https://a.test/x", 201, "HTTP/2", [("x-a", "1")], b"<p>hi</p>")
    back = pickle.loads(pickle.dumps(r))
    assert (back.url, back.status, back.version, back.headers, back.body) == (
        "https://a.test/x",
        201,
        "HTTP/2",
        [("x-a", "1")],
        b"<p>hi</p>",
    )
    assert netweir.Page(back).css("p::text").get() == "hi"


ITEMS = 12


class Handler(BaseHTTPRequestHandler):
    redesigned = False

    def log_message(self, *args):
        pass

    def do_GET(self):
        if self.path == "/":
            links = "".join(f'<a class="item" href="/item/{i}">i</a>' for i in range(ITEMS))
            body = f"<html><body>{links}</body></html>"
        elif self.path.startswith("/item/"):
            body = f"<h1>{self.path.rsplit('/', 1)[1]}</h1>"
        elif self.path.startswith("/price"):
            cls = "cost" if Handler.redesigned else "price_color"
            body = f'<div><p class="{cls}">£51.77</p></div>'
        else:
            body = "<p>x</p>"
        data = body.encode()
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


def settings(**kw):
    return netweir.Settings(
        throttle=False, start_delay=0, obey_robots=False, obey_tdmrep=False, **kw
    )


def crawl(spider_class, url, **kw):
    items = []
    spider = spider_class()
    spider.start_urls = [url]
    spider.settings = settings(**kw)
    stats = spider.run(output=items.append)
    return items, stats


def test_workers_write_what_one_process_writes(base, tmp_path):
    one, _ = crawl(worker_spiders.Shop, base + "/", checkpoint=str(tmp_path / "one"))
    many, stats = crawl(
        worker_spiders.Shop, base + "/", workers=3, checkpoint=str(tmp_path / "many")
    )

    # Pages finish fetching in a different order each run, workers or not;
    # what must match is every item and its id.
    def strip(items):
        rows = [{k: v for k, v in i.items() if k != "pid"} for i in items]
        return sorted(rows, key=lambda r: r["_id"])

    assert strip(many) == strip(one), "the same items, with the same ids"
    pids = {i["pid"] for i in many if "pid" in i}
    assert len(pids) > 1 and os.getpid() not in pids, pids
    assert len(many) == 2 * ITEMS
    assert stats["callback_errors"] == 0


def test_a_callback_that_breaks_in_a_worker_is_counted_and_logged(base, caplog):
    with caplog.at_level(logging.ERROR, logger="netweir"):
        items, stats = crawl(worker_spiders.Breaks, base + "/", workers=2)
    assert items == [{"before": base + "/"}], "what came before the error is kept"
    assert stats["callback_errors"] == 1
    assert "ValueError: broke on" in caplog.text


def test_fail_fast_stops_the_crawl_with_the_workers_exception(base):
    with pytest.raises(ValueError, match="broke on"):
        crawl(worker_spiders.Breaks, base + "/", workers=2, fail_fast=True)


def test_tracked_selectors_relocate_in_workers_and_say_so_once(base, tmp_path, caplog):
    Handler.redesigned = False
    crawl(worker_spiders.Tracks, base + "/price", workers=2, checkpoint=str(tmp_path / "s"))
    Handler.redesigned = True
    try:
        with caplog.at_level(logging.WARNING, logger="netweir"):
            items, _ = crawl(
                worker_spiders.Tracks, base + "/price?2", workers=2, checkpoint=str(tmp_path / "s")
            )
    finally:
        Handler.redesigned = False
    assert items[0]["price"] == "£51.77"
    assert caplog.text.count("found by similarity") == 1


def test_a_spider_defined_in_a_function_is_refused_with_workers(base):
    class Local(netweir.Spider):
        pass

    with pytest.raises(TypeError, match="module level"):
        crawl(Local, base + "/", workers=2)


def test_workers_must_be_at_least_one():
    with pytest.raises(ValueError, match="workers"):
        netweir.Settings(workers=0)


def test_the_command_runs_a_files_spider_in_workers(base, tmp_path):
    script = tmp_path / "spider.py"
    script.write_text(
        "import os\nimport netweir\n\n"
        "class Files(netweir.Spider):\n"
        f"    start_urls = ['{base}/']\n"
        "    def parse(self, page):\n"
        "        for href in page.css('a.item::attr(href)').getall():\n"
        "            yield page.follow(href, callback='item')\n"
        "    def item(self, page):\n"
        "        yield {'name': page.css('h1::text').get(), 'pid': os.getpid()}\n",
        encoding="utf-8",
    )
    out = tmp_path / "items.jsonl"
    done = subprocess.run(
        [sys.executable, "-m", "netweir", "crawl", str(script), "-q", "-o", str(out),
         "-s", "workers=2", "-s", "throttle=false", "-s", "start_delay=0",
         "-s", "obey_robots=false", "-s", "obey_tdmrep=false"],
        capture_output=True, text=True, timeout=120,
    )  # fmt: skip
    assert done.returncode == 0, done.stderr
    rows = out.read_text(encoding="utf-8").splitlines()
    assert len(rows) == ITEMS


@pytest.mark.skipif(sys.platform == "win32", reason="uses pgrep")
def test_workers_go_when_the_crawl_is_killed(base, tmp_path):
    import shutil
    import signal
    import time

    if not shutil.which("pgrep"):
        pytest.skip("needs pgrep")
    script = tmp_path / "slow.py"
    script.write_text(
        "import time\nimport netweir\n\n"
        "class Slow(netweir.Spider):\n"
        f"    start_urls = ['{base}/']\n"
        "    def parse(self, page):\n"
        "        time.sleep(60)\n"
        "        yield {}\n",
        encoding="utf-8",
    )
    crawl = subprocess.Popen(
        [sys.executable, "-m", "netweir", "crawl", str(script), "-q", "-s", "workers=2",
         "-s", "obey_robots=false", "-s", "obey_tdmrep=false"],
    )  # fmt: skip

    def children():
        found = subprocess.run(["pgrep", "-P", str(crawl.pid)], capture_output=True, text=True)
        return found.stdout.split()

    deadline = time.monotonic() + 30
    while not children() and time.monotonic() < deadline:
        time.sleep(0.1)
    workers = children()
    assert workers, "the crawl started workers"
    os.kill(crawl.pid, signal.SIGKILL)
    crawl.wait()
    deadline = time.monotonic() + 10
    alive = workers
    while alive and time.monotonic() < deadline:
        time.sleep(0.2)
        alive = [w for w in workers if _running(int(w))]
    assert not alive, f"workers outlived the crawl: {alive}"


def _running(pid: int) -> bool:
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    # A zombie still answers; it has exited, which is what matters.
    state = subprocess.run(["ps", "-o", "stat=", "-p", str(pid)], capture_output=True, text=True)
    return bool(state.stdout.strip()) and not state.stdout.strip().startswith("Z")
