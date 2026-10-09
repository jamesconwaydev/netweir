# Redesign pairs

Each case is a page before and after a redesign, the selector that found the
element before, and what the element says after (or nothing, when it's
gone). `tests/track.rs` fingerprints the element on the first page, looks
for it on the second, and checks the answer; the scoring weights in
`src/track.rs` are tuned until every case passes with the default threshold
of 0.75.

Written by hand, modelled on the changes sites actually make: classes
renamed by a new build, wrappers added, tags swapped, lists reordered,
prices changed. Two cases are the other way round: an element that is simply
absent this time (a sold-out product's price, a table without its Total
row) must not be replaced by a neighbour that looks like it.
