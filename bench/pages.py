"""Builds the benchmark pages: a shop listing, like most pages people scrape.

Generated rather than downloaded, so the numbers are reproducible and the
repository holds no third-party content.
"""

import random

# fmt: off
WORDS = [
    "light", "bright", "hollow", "river", "stone", "paper", "glass", "winter", "amber",
    "copper", "quiet", "field", "garden", "harbour", "lantern", "meadow", "north",
    "orchard", "silver", "timber", "velvet", "willow",
]
# fmt: on


def _words(rng: random.Random, n: int) -> str:
    return " ".join(rng.choice(WORDS) for _ in range(n))


def product(rng: random.Random, i: int) -> str:
    stock = "In stock" if rng.random() > 0.2 else "Sold out"
    rating = rng.choice(["One", "Two", "Three", "Four", "Five"])
    return f"""
<article class="product_pod" data-id="{i}">
  <div class="image_container"><a href="/catalogue/item-{i}/index.html"><img src="/media/{i}.jpg" alt="{_words(rng, 3)}" class="thumbnail"></a></div>
  <p class="star-rating {rating}"><i class="icon-star"></i><i class="icon-star"></i></p>
  <h3><a href="/catalogue/item-{i}/index.html" title="{_words(rng, 5).title()}">{_words(rng, 4).title()}</a></h3>
  <div class="product_price">
    <p class="price_color">£{rng.uniform(5, 90):.2f}</p>
    <p class="instock availability"><i class="icon-ok"></i> {stock}</p>
    <form><button type="submit" class="btn btn-primary btn-block" data-loading-text="Adding...">Add to basket</button></form>
  </div>
  <p class="description">{_words(rng, rng.randint(20, 60))}</p>
</article>"""


def page(target_bytes: int, seed: int = 7) -> str:
    rng = random.Random(seed)
    head = (
        "<!doctype html><html lang=en><head><meta charset=utf-8><title>Shop</title>"
        + "".join(f'<link rel=stylesheet href="/static/{i}.css">' for i in range(10))
        + "</head><body><header><nav><ul>"
        + "".join(f'<li><a href="/c/{w}">{w.title()}</a></li>' for w in WORDS)
        + "</ul></nav></header><main><ol class=row>"
    )
    parts = [head]
    size = len(head)
    i = 0
    while size < target_bytes:
        item = f"<li>{product(rng, i)}</li>"
        parts.append(item)
        size += len(item)
        i += 1
    parts.append(
        '</ol><ul class=pager><li class=next><a href="page-2.html">next</a></li></ul></main></body></html>'
    )
    return "".join(parts)
