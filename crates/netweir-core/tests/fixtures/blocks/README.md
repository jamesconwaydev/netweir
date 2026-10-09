# Block page fixtures

One HTTP response per file: the status line, headers, a blank line, then the
body. `tests/blocks.rs` classifies each and checks the outcome.

These are written by hand from each vendor's public documentation and from
block pages people have published, trimmed to the parts the signatures look
at. They are not captures of live traffic: the tests never contact the
vendors. When a vendor changes its pages, update the fixture and the
signature in `signatures/blocks.toml` together.

The `ok-*`, `not-found`, `throttled`, `unavailable` and `payment` files are
the other side: pages that mention a vendor or sit behind one without being
a block.
