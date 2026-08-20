# Third-party notices

OxiMux is Apache-2.0. This file records third-party work that is **derived from
rather than depended on** — technique and constants that were read and adapted,
where nothing appears in `Cargo.toml` and the debt would otherwise leave no
trace.

Ordinary dependencies are not listed here. Their licences travel with them in
`Cargo.lock` and are reproduced by `cargo about` at release time.

## Transcript follow spring — MIT-licensed reference implementation

`apps/desktop/src/shell/agent_chat/stick_spring.rs` takes its shape — a velocity
spring with a feed-forward estimate of how fast the content end is moving — and
six starting constants from an MIT-licensed chat client, by way of the phase plan
that recorded them. No source was copied; what transferred is a technique and a
handful of numbers, several of which have since been dropped or reshaped.

What is **not** derived: the lag formulation, the item-index re-anchoring that
makes it work against a lazily-measured `gpui::list`, and the settle grace. Those
are what it took to drive this list rather than a plain scroll container.

MIT permits reuse with attribution, and this entry exists because an integration
loop adapted at all deserves recording — even though six floats and a shape are
very unlikely to be "a substantial portion of the Software" the licence's
condition attaches to, which is the only thing that would make credit an
obligation rather than a courtesy.

The upstream project is named in this work's phase plan rather than here, and the
licence text is not reproduced because that project is not vendored and was never
read directly. If it is ever vendored, replace this section with its verbatim
notice and copyright line.
