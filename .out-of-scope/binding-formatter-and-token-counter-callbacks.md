# Formatter and TokenCounter callbacks in the bindings

The Python and Node bindings will not let a consumer replace the `Formatter` or the `TokenCounter` with a function in their own language. A binding names one of the built-in formats, and `context` sizes rows with the conservative bytes-based estimate. A Rust consumer can still implement either trait (#23).

## Why this is out of scope

No consumer has asked for either callback. The stories that asked for them (#49 stories 23 to 25) came from the bindings spec itself.

- **Formatter.** The four built-ins (flat TOON per Kind, compact JSON, Markdown and pretty JSON) cover the common shapes. `context` returns the rows it selected alongside the text (ADR-0016), so a consumer who wants another shape builds it from the rows in their own language, with no callback and no FFI round trip per row.
- **TokenCounter.** The counter is called once per candidate row, up to 100 times per `context` call, so each call would cross the FFI that many times. The bytes-based estimate already guarantees the budget never overflows. An exact counter only recovers budget the estimate leaves unused.

The budget is measured in the chosen format. A consumer whose own shape is longer than that format picks the closest built-in or leaves headroom in the budget.

## When to reconsider

- A consumer needs a format that cannot be built from the returned rows within the budget, and no built-in is close enough.
- A consumer shows that the bytes-based estimate wastes enough budget to change what their model sees, and an exact counter in Rust for their model is not an option.
- Callback latency is measured and is small beside the cost of `context`.

## Prior requests

- #60: "Sync callbacks for Formatter and TokenCounter"
- Design review of 2026-11-26, finding 5 (`docs/reviews/2026-11-26-design-review.md`)
