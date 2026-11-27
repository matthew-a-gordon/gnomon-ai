# Research: time libraries for the Rust core and its bindings

Checked on 2026-11-27. The question is whether `jiff` 1.0, or any other alternative to `chrono`, can work with all four things gnomon needs: `tokio-postgres`, PyO3, napi-rs and the `schemars` derive on shared types ([#50](https://github.com/matthew-a-gordon/gnomon-ai/issues/50), ADR-0016, ADR-0017). The claim to test was that nothing other than the current choice can work.

This report describes the code before [#110](https://github.com/matthew-a-gordon/gnomon-ai/pull/110) moved to `jiff` 1.0. Below, "the current choice" and "this branch" mean that earlier state. #110 then added gnomon's own Postgres conversion, in `crates/gnomon/src/timestamptz.rs`.

Release dates and versions come from the crates.io API. Library source was read from the published `.crate` files. Every conversion verdict below was compiled with Rust 1.93.1 and run, except where a line says it was not.

## Verdict

The claim is false.

- `jiff` 1.0.3 works in all four places. A hand-written newtype ran microsecond round trips through a real `timestamptz`, CPython, Node and a `schemars` derive. The Python and Node modules also ran end to end to Postgres.
- `jiff` 0.2, `time` 0.3, `chrono`, two `chrono` forks, `std::time::SystemTime`, `hifitime`, `temporal_rs` and a bare `i64` of microseconds also work in all four places, at the levels the matrix shows.
- No candidate has a built-in conversion in all four places that keeps microseconds. napi 3.16.3 converts only `chrono`, and only to a JavaScript `Date`, which holds whole milliseconds. This includes the current choice.
- No pair of candidate and library is impossible. Three representations are impossible for every candidate: microseconds in a JavaScript `Date`, `infinity` in a Python `datetime` or a `Date`, and a year outside 1 to 9999 in a Python `datetime`.

The checked-out branch (`feat/jiff-times`, commit `b66f1c1`) uses `jiff` 0.2. `main` uses `chrono`. The verdict holds whichever of the two is called the current choice.

A narrower claim is true. Among maintained candidates, `jiff` 0.2 is the only one whose built-in conversions for Postgres, Python and JSON Schema all kept microseconds across 1969 and 2024 and wrote a date-time schema. `time` has no JSON Schema conversion. The `SystemTime` conversions fail before 1970 in Python and in JSON. `chrono` is unmaintained. The claim counts `jiff` 0.2 as maintained, although no release or commit on its branch followed `jiff` 1.0.0. It still needs a hand-written Node conversion, and its Python conversion has the limits in section 7.

## Matrix

Levels, kept separate:

- (a) built in to the latest release.
- (b) merged or proposed upstream and not released. `deny.toml` allows only crates.io, so gnomon cannot ship it.
- (c) a third-party adapter crate on crates.io.
- (d) a hand-written adapter in gnomon.
- (e) impossible.

A cell lists the levels that were established. A hand-written newtype is always an option, so (d) is listed only where it is needed or was run. Every (a), (c) and (d) entry was run.

| Candidate | Maintained, RustSec | tokio-postgres | PyO3 | napi-rs | schemars |
|---|---|---|---|---|---|
| `jiff` 1.0.3 | Yes. None | b, d | b, d | d | b, c, d |
| `jiff` 0.2.46 | Last release 2026-11-15. None | a | a (limits, section 7) | d | a |
| `time` 0.3.55 | Yes. Two past ones, patched | a (panic and feature traps, section 6) | a (rejects `zoneinfo`) | d | b (not built), c (integers only), d |
| `chrono` 0.4.45 | No. RUSTSEC-2026-0543 | a | a (UTC or fixed offset only) | a in milliseconds only, d | a |
| `sad-chrono` 0.5.2 | Nine days old. None in RustSec | d (shim) | d (shim) | d (shim, plus bigint) | d (shim) |
| `ironchrono` 0.4.46 | Three days old. None | d (shim) | d (shim) | d (shim, plus bigint) | d (shim) |
| `std::time::SystemTime` | Standard library | a (reads `infinity` as a finite time), d | a from 1970 only, d | d | a (wrong shape, 1970 on), c, d |
| `hifitime` 4.3.2 | Yes. None | d | d | d | d |
| `temporal_rs` 0.2.6 | Yes. None | d | d | d | d |
| Own `i64` microseconds | Not a crate | d | d | d | d |

Level (e), for every candidate:

| Representation | Why it is impossible |
|---|---|
| Microseconds in a JavaScript `Date` | A time value is a whole number of milliseconds. [`TimeClip`](https://tc39.es/ecma262/multipage/numbers-and-dates.html#sec-timeclip) truncates it, and Node-API creates a `Date` from [milliseconds](https://nodejs.org/api/n-api.html#napi_create_date). Run: `new Date(1.999).getTime()` is `1`. A `bigint`, a string or a `Temporal.Instant` carries microseconds instead. |
| `infinity` in a Python `datetime` or a `Date` | [`datetime`](https://docs.python.org/3.14/library/datetime.html#constants) spans years 1 to 9999 and has no infinite value. `TimeClip` turns a non-finite number into `NaN`, an invalid `Date`. A binding has to map `infinity` to `None`, a tagged value or an error. |
| A year outside 1 to 9999 in a Python `datetime` | `MINYEAR` is 1 and `MAXYEAR` is 9999. Postgres holds 4713 BC to 294276 AD ([documentation](https://www.postgresql.org/docs/17/datatype-datetime.html)). |

## 1. Candidates, maintenance and advisories

Advisories were read from [rustsec/advisory-db](https://github.com/rustsec/advisory-db) at `c5279d0` (2026-11-27 10:35 UTC). Versions are the latest stable on [crates.io](https://crates.io).

| Crate | Latest, date | State | Advisories |
|---|---|---|---|
| [`jiff`](https://crates.io/crates/jiff) | 1.0.3, 2026-11-24 (1.0.0 on 2026-11-15) | Active | None |
| `jiff` 0.2 | 0.2.46, 2026-11-15 | The `release/v02` branch ends at that release. No support policy for 0.2 was found | None |
| [`time`](https://crates.io/crates/time) | 0.3.55, 2026-08-01 | Active (repository pushed 2026-10-16) | RUSTSEC-2020-0071 (patched 0.2.23), RUSTSEC-2026-0009 (patched 0.3.47) |
| [`chrono`](https://crates.io/crates/chrono) | 0.4.45, 2026-06-04 | Unmaintained. Last commit 2026-08-03 | [RUSTSEC-2026-0543](https://rustsec.org/advisories/RUSTSEC-2026-0543.html) (unmaintained, no patched version), RUSTSEC-2020-0159 (patched 0.4.20) |
| [`sad-chrono`](https://crates.io/crates/sad-chrono) | 0.5.2, 2026-11-23 | Fork. First release 2026-11-18, one publisher | None in RustSec. Its [advisory PR](https://github.com/rustsec/advisory-db/pull/3613) was closed because the crate is below the notability criteria. That PR lists five GitHub advisories of its own, fixed by 0.5.1 |
| [`ironchrono`](https://crates.io/crates/ironchrono) | 0.4.46, 2026-11-24 | Fork. One release, 7 downloads | None |
| [`hifitime`](https://crates.io/crates/hifitime) | 4.3.2, 2026-11-03 | Active | None |
| [`temporal_rs`](https://crates.io/crates/temporal_rs) | 0.2.6, 2026-08-05 | Active (repository pushed 2026-11-13) | None |

About RUSTSEC-2026-0543:

- The advisory recommends `jiff` or `time`.
- The maintainer announced the wind-down in [chronotope/chrono#1768](https://github.com/chronotope/chrono/issues/1768). The maintainer wrote that they would consider a successor "strictly on my discretion", and [turned one volunteer down](https://github.com/chronotope/chrono/issues/1768#issuecomment-5242132087).
- The advisory was [held for `jiff` 1.0](https://github.com/chronotope/chrono/issues/1768#issuecomment-5681537759) and merged in [advisory-db#3214](https://github.com/rustsec/advisory-db/pull/3214) on 2026-11-17.

`cargo deny check advisories` with the repository's `deny.toml` flagged only `chrono` in the probe projects that held `jiff` 0.2 and 1.x, `time`, `hifitime` and `temporal_rs`. It passed the two fork projects, where the local shim is named `chrono` and has no registry source. A passing check there says nothing about the fork's safety.

Search for other crates: crates.io queries for `jiff`, `chrono`, `datetime` and `timestamp`, and the `date-and-time` category by recent downloads. `icu_calendar` and `icu_time` 2.4.0, `speedate` 0.18.0 (a parser) and `tz-rs` 0.7.3 were seen and not examined.

## 2. Level (a): what the latest releases convert

One crate asserted each trait for each type, with every time feature on: `with-jiff-0_2`, `with-time-0_3`, `with-chrono-0_4`, PyO3 `jiff-02`, `time` and `chrono`, napi `chrono_date`, and `schemars` `jiff02` and `chrono04`. Versions: `tokio-postgres` 0.7.18, `postgres-types` 0.2.14, `pyo3` 0.30.1, `napi` 3.16.3, `schemars` 1.2.3.

```
cargo +1.93.1 build --message-format=json   # every failure was E0277

built-in trait impl present in the latest release ('yes' compiles, '-' fails with E0277)
                pg_bind  pg_decode      py_in     py_out    node_in   node_out     schema
jiff1                 -          -          -          -          -          -          -
jiff02              yes        yes        yes        yes          -          -        yes
time                yes        yes        yes        yes          -          -          -
chrono              yes        yes        yes        yes        yes        yes        yes
system              yes        yes        yes        yes          -          -        yes
hifitime              -          -          -          -          -          -          -
temporal_rs           -          -          -          -          -          -          -
```

`time` here is `OffsetDateTime`, `chrono` is `DateTime<Utc>`, `hifitime` is `Epoch` and `temporal_rs` is `Instant`.

Source for the built-ins:

- `postgres-types`: [`jiff_02.rs`](https://docs.rs/crate/postgres-types/0.2.14/source/src/jiff_02.rs), [`time_03.rs`](https://docs.rs/crate/postgres-types/0.2.14/source/src/time_03.rs), [`chrono_04.rs`](https://docs.rs/crate/postgres-types/0.2.14/source/src/chrono_04.rs), `SystemTime` in [`lib.rs`](https://docs.rs/crate/postgres-types/0.2.14/source/src/lib.rs) (lines 794 and 1241).
- PyO3: [`conversions/jiff.rs`](https://docs.rs/crate/pyo3/0.30.1/source/src/conversions/jiff.rs), [`time.rs`](https://docs.rs/crate/pyo3/0.30.1/source/src/conversions/time.rs), [`chrono.rs`](https://docs.rs/crate/pyo3/0.30.1/source/src/conversions/chrono.rs), [`std/time.rs`](https://docs.rs/crate/pyo3/0.30.1/source/src/conversions/std/time.rs).
- napi: [`js_values/date.rs`](https://docs.rs/crate/napi/3.16.3/source/src/bindgen_runtime/js_values/date.rs). Its [`Cargo.toml`](https://docs.rs/crate/napi/3.16.3/source/Cargo.toml) names no time crate but `chrono`.
- `schemars`: [`jiff02.rs`](https://docs.rs/crate/schemars/1.2.3/source/src/json_schema_impls/jiff02.rs), [`chrono04.rs`](https://docs.rs/crate/schemars/1.2.3/source/src/json_schema_impls/chrono04.rs), [`std_time.rs`](https://docs.rs/crate/schemars/1.2.3/source/src/json_schema_impls/std_time.rs).

`hifitime`'s `python` feature is not a conversion gnomon can use. It exposes `hifitime`'s own Python class and [pins PyO3 0.29](https://docs.rs/crate/hifitime/4.3.2/source/Cargo.toml). Adding it beside PyO3 0.30.1 fails to resolve, because only one package may link `pyo3-python`.

## 3. Level (b): unreleased support

| Library | For | State on 2026-11-27 | Run here |
|---|---|---|---|
| `tokio-postgres` | `jiff` 1 | [rust-postgres#1405](https://github.com/rust-postgres/rust-postgres/pull/1405), open since 2026-11-16, no review or comment, head `7fbb21a` | Yes |
| PyO3 | `jiff` 1 | [PyO3#6584](https://github.com/PyO3/pyo3/pull/6584) merged 2026-11-23, with a fix in [#6606](https://github.com/PyO3/pyo3/pull/6606). A contributor wrote in [#6609](https://github.com/PyO3/pyo3/issues/6609) that it "won't be available until `0.31`" | Yes, `main` at `1430129` |
| `schemars` | `jiff` 1 | [schemars#547](https://github.com/GREsau/schemars/pull/547), open since 2026-11-18, head `78ff114` | Yes |
| `schemars` | `time` 0.3 | [#472](https://github.com/GREsau/schemars/pull/472) (2025) and [#257](https://github.com/GREsau/schemars/pull/257) (2023), both open | No |
| napi-rs | `jiff` | [napi-rs#2969](https://github.com/napi-rs/napi-rs/issues/2969) is a request, open since 2025-10-22. No pull request was found for `jiff` or `time` | Nothing to run |

Sources were downloaded as tarballs at those commits and used as path dependencies.

```
### rust-postgres PR 1405 @ 7fbb21a
timestamptz[] with NULL exact: true
sub-us before 1970: 1969-07-21T02:56:14.876543211Z stored as 1969-07-21T02:56:14.876544Z
infinity: Err("error deserializing column 0")
infinity wrapped: PosInfinity
### schemars PR 547 @ 78ff114
required: ["at","history"]
properties: {"at":{"format":"date-time","type":"string"},"history":{"items":{"format":"date-time","type":["string","null"]},"type":"array"},"retired":{"format":"date-time","type":["string","null"]}}
### PyO3 main @ 1430129
random UTC round trip exact: True
zoneinfo fold=1 2024-11-03T06:30:00+00:00 exact
naive TypeError expected a datetime with non-None tzinfo
datetime.max ValueError error converting datetime to instant in time zone UTC: ...
```

## 4. Level (c): third-party adapter crates

One was found that matters: [`serde_with`](https://crates.io/crates/serde_with) 3.25.0 (2026-11-19). With `schemars_1` and `jiff_1` or `time_0_3` it covers serde and the schema together. It covers neither Postgres, Python nor Node.

| Use | Schema it writes (run) |
|---|---|
| `DisplayFromStr` on a `jiff` 1 field | `{"type":"string"}`, no `format` ([source](https://docs.rs/crate/serde_with/3.25.0/source/src/schemars_1.rs), line 407) |
| `TimestampMicroSeconds<i64>` on `jiff` 1, `time` or `SystemTime` | `{"type":"integer","format":"int64"}`. Round trips exactly, before 1970 too. A JavaScript client that parses it as a number loses precision after year 2255 |

No adapter crate for a `jiff` 1 `timestamptz`, Python or Node conversion was found. The search:

- 35 likely names such as `pyo3-jiff`, `napi-jiff`, `schemars-time` and `hifitime-postgres` returned 404.
- `jiff` had 2561 reverse dependencies, 656 on a `^1` requirement. Each of those was checked in the sparse index for a normal dependency on `tokio-postgres`, `postgres-types`, `postgres-protocol`, `pyo3`, `napi` or `schemars`.
- Of the matches that could have been adapters, [`orphanage`](https://crates.io/crates/orphanage) 0.5.8 converts only `INTERVAL`. [`vitaminc-aead-napi`](https://crates.io/crates/vitaminc-aead-napi) is a bridge for an encryption library. `autumn-harvest` 0.11.0 wraps timestamps for Diesel, not for `postgres-types`. A text search of the other `tokio-postgres` and PyO3 matches found no timestamp conversion.
- `zero-postgres` 0.11.0 has a `jiff` 1 feature, but it is a different driver.
- The reverse dependencies of `hifitime` held no Postgres, Python, Node or schema adapter by name. The reverse dependencies of `time` were not scanned.

## 5. Level (d): what a hand-written adapter costs

### The orphan rule

gnomon cannot implement a foreign trait on a foreign type. All four attempts for `jiff` 1 failed:

```
error[E0117]: only traits defined in the current crate can be implemented for types defined outside of the crate
  (x4: JsonSchema, ToNapiValue, IntoPyObject, ToSql)
```

So an adapter is a newtype (`struct Ts(jiff::Timestamp)`) with the impls beside it. Two consequences follow.

- The bindings are separate crates. A binding crate cannot implement PyO3 or napi traits on a newtype the core defines, because both are foreign to it. Either the core carries the impls behind Cargo features, or each binding declares its own newtype and mirrors the shared types.
- Either the public types hold the newtype, or they hold `jiff::Timestamp` and each boundary wraps it. On this branch `transaction.rs` has 26 lines that name `Type::TIMESTAMPTZ` and 31 calls that decode a time. The public types have 33 time fields or variants.

### Size for `jiff` 1

The appendix holds the whole adapter. Counting lines that are neither blank nor comments:

| Part | Lines | What the derives or generators still need |
|---|---|---|
| Postgres (`ToSql`, `FromSql`) | 28 | Nothing. `Option<T>`, `Vec<T>`, `&[T]` and `postgres_types::Timestamp<T>` work through the generic impls |
| Python (`FromPyObject`, `IntoPyObject`) | 64 | `INPUT_TYPE` and `OUTPUT_TYPE`, or the stubs say `Incomplete` |
| Node (`bigint`) | 33 | `ts_args_type`, `ts_return_type` or `ts_type` on every function and field, or the `.d.ts` does not type-check |
| serde and `JsonSchema` | 31 | Nothing on fields that use the newtype |

A shorter route converts through `jiff` 0.2 by nanoseconds. It was run for Python (about 20 lines) and inherits the 0.2 conversion's limits.

`Timestamp::from_microsecond` rejected valid values before `jiff` 1.0.3 ([jiff#723](https://github.com/BurntSushi/jiff/issues/723)), so an adapter that uses it should require 1.0.3.

### Forks through a shim

A dependency renamed to `chrono` does not satisfy the `chrono` features of other crates, because the types differ ([sad-chrono README](https://docs.rs/crate/sad-chrono/0.5.2/source/README.md)). A local package named `chrono` that holds `pub use sad_chrono::*;` (or `ironchrono`), applied with `[patch.crates-io]`, does satisfy them. The probe projects were rebuilt that way. The `chrono` sections of the Postgres, Python, Node and schema output matched upstream `chrono`, apart from one object address.

[Cargo reads `[patch]` only from the root workspace](https://doc.rust-lang.org/cargo/reference/overriding-dependencies.html). A Rust program that depends on gnomon would get upstream `chrono` unless it patched too. That is why this is level (d) and not (a).

## 6. Proof: tokio-postgres

PostgreSQL 17.11 from `pgvector/pgvector:pg17`, bound to 127.0.0.1, unnamed statements through `query_typed`. Each value was compared with the server's own count (`extract(epoch ...)` for the samples, `timestamptz_send` at the range limits), so an error made in both directions could not pass.

```
docker run -d --name time-research-pg -e POSTGRES_PASSWORD=... -p 127.0.0.1::5432 pgvector/pgvector:pg17
PG_URL=... cargo +1.93.1 run --bin pg
```

Output for the `jiff` 1 newtype:

```
scalar: 2016/2016 exact to the microsecond; first mismatches []
option: None bound as null=true, decoded=None, Some decoded ns=Some(1000)
array: postgres sees [1,NULL,-1,1710408600123456]; decoded [Some(1), None, Some(-1), Some(1710408600123456)]
array: &[T] of 2016 round-trips exactly: true
array: empty len=0, null array decoded as None=true
array: NULL element into Vec<T>: error deserializing column 0: a Postgres value was `NULL`
sub-us after 2000 : given_ns=1710408600123456789 stored_us=1710408600123456
sub-us before 2000: given_ns=-14182939876543211 stored_us=-14182939876543
decode infinity: Err error deserializing column 0: value too large to decode
decode infinity as postgres_types::Timestamp<T>: Ok(PosInfinity)
bind PosInfinity: postgres stores 'infinity'
array of Option<Timestamp<T>> with infinity and NULL: [Some(PosInfinity), None, Some(Value(Ts(1970-01-01T00:00:00Z)))]
...
adapter vs built-in jiff 0.2 on sub-microsecond input: 3000/3000 stored identically
```

The first eight of these lines were the same for `jiff` 0.2, `time`, `chrono`, `SystemTime` (built in and newtype), `hifitime`, `temporal_rs` and the own `i64`, with one difference: the `i64` type cannot hold digits below a microsecond. `-infinity` behaved as `infinity` did.

### Range

Postgres accepted 4714-11-24 00:00:00 BC to 294276-12-31 23:59:59.999999 and refused a value one step outside each end. Every candidate decoded the low end exactly. The high end differs.

| Type | Largest instant it holds | First failure on decode |
|---|---|---|
| `jiff` 1.0.3 and 0.2.46 | 9999-12-30 22:00:00.999999999 | 9999-12-30 22:00:01 |
| `time` `OffsetDateTime` | 9999-12-31 23:59:59.999999999 | 10000-01-01 |
| `time` with `large-dates` | Covers the Postgres range | None |
| `chrono` | 262142-12-31 | 262143-01-01 |
| `temporal_rs` | ECMAScript's 100,000,000 days | 294276-12-31 (262143-01-01 passed) |
| Own `i64` from 1970 | About year 294247 | 294276-12-31. Counting from 2000 would avoid it |
| `SystemTime` (Linux), `hifitime` | Covers the Postgres range | None |

A decode failure is an error, `value too large to decode` for the built-ins. Binding the minimum of `jiff`, `time` or `chrono` is refused by the server with `timestamp out of range`.

### `infinity`

| Type | Reading `infinity` as that type |
|---|---|
| `jiff` 0.2, `jiff` 1 newtype, `time`, `chrono` | Error |
| `time` with `large-dates` | Returns `+294277-01-09 4:00:54.775807` without an error |
| `SystemTime` built in | Returns a finite time without an error |
| `SystemTime`, `hifitime`, `temporal_rs` newtypes | Error, because the adapter checks for it |
| Own `i64` | Keeps the sentinel, by choice |
| `postgres_types::Timestamp<T>` around any of them | `PosInfinity` or `NegInfinity`, and it binds back |

`large-dates` is a Cargo feature, so any crate in the graph can switch it on for everyone.

### Two panics in built-in conversions

```
bind time 9999-12-31T23:59:59-05:00
  panicked at postgres-types-0.2.14/src/time_03.rs:52:33: local datetime out of valid range
bind SystemTime at i64::MAX seconds
  debug build:   panicked at postgres-types-0.2.14/src/lib.rs:1246:27: attempt to multiply with overflow
  release build: stored '1969-12-31 23:59:58+00'
```

The first happens in debug and release. The value is valid in its own offset and falls in year 10000 in UTC, where [`to_offset`](https://docs.rs/crate/time/0.3.55/source/src/offset_date_time.rs) panics. PyO3's `time` conversion accepts such a `datetime`. The path from Python to that panic was not run as one program. The second needs an absurd value.

## 7. Proof: PyO3

CPython 3.14.10, PyO3 0.30.1, maturin 1.16.0. Python compared each result with its own `(dt - epoch) // timedelta(microseconds=1)`.

```
maturin build -i venv/bin/python -o dist && uv pip install --python venv/bin/python dist/*.whl
python -I py_probe.py
PG_URL=... python -I py_e2e.py
```

Every conversion passed 3009 of 3009 random and edge instants in both directions, except built-in `SystemTime`, which passed the 2463 that are 1970 or later.

End to end, `jiff` 1 newtype, from Python through `timestamptz` and `timestamptz[]` and back:

```
scalar timestamptz: True
timestamptz[] with NULL: 2004 elements, 2004 exact
None scalar: (None, [])
postgres text for +05:30 input: 2024-03-14 09:30:00.000007 {"2024-03-14 09:30:00.000007"}
```

The newtype also worked as a field of a derived struct and of a `#[pyclass]`:

```
derive(FromPyObject, IntoPyObject) round trip: True
pyclass get: (True, None, True)
pyclass set: True
```

The module was also built as an `abi3-py310` wheel. On CPython 3.14.10 it printed the same results. On 3.10.22 only the text of Python's own out-of-range errors differed. The end-to-end run passed on both.

What each conversion accepts as input. All reject `date` and `str`. All but the built-in `SystemTime` reject a naive `datetime` with their own `TypeError`. The built-in `SystemTime` fails in the subtraction.

| Conversion | Fixed offset | `ZoneInfo` | Custom `tzinfo` | Returned offset | Python `datetime.max` |
|---|---|---|---|---|---|
| `jiff` 1 newtype, direct | Yes | Yes | Yes | UTC | `ValueError` |
| `jiff` 1 through 0.2 | Yes | By `key` only | No | UTC | `ValueError` |
| `jiff` 0.2 built in | Yes | By `key` only | No | UTC | `ValueError` |
| `time` `OffsetDateTime` | Yes | No | No | As given | Yes |
| `time` `UtcDateTime` | No | No | No | UTC | Yes |
| `chrono` `DateTime<Utc>` | No (only `timezone.utc`) | No | No | UTC | Yes |
| `chrono` `DateTime<FixedOffset>` | Yes | No | No | As given | Yes |
| `SystemTime` built in | Yes, 1970 on | Yes, 1970 on | Yes, 1970 on | UTC | Yes |
| `SystemTime`, `hifitime`, `temporal_rs`, `i64` through the newtype | Yes | Yes | Yes | UTC | `ValueError` |

`SystemTime` built in fails before 1970 in both directions. Input raises `ValueError`. Output panics on [`duration_since(UNIX_EPOCH).unwrap()`](https://docs.rs/crate/pyo3/0.30.1/source/src/conversions/std/time.rs) (line 123):

```
out make(-1 us): RAISES PanicException: called `Result::unwrap()` on an `Err` value: SystemTimeError(1µs)
```

The `jiff` 0.2 conversion looks a `ZoneInfo` up by name in `jiff`'s time zone database and ignores the offset Python computes. To force a disagreement, the probe built `ZoneInfo.from_file(paris_file, key="America/New_York")`:

```
python offset: 2:00:00
jiff02: differs from Python's instant by 6.0 h
jiff1_via02: differs from Python's instant by 6.0 h
jiff1_adapter: differs from Python's instant by 0.0 h
pyo3 main jiff 1 feature: differs from Python instant by 6.0 h
```

In normal use the two give different instants only if they read different time zone data, such as two versions of it. This conversion also rejects a `ZoneInfo` that has no key.

### Stubs

`maturin build --generate-stubs` with `experimental-inspect`, on a declarative `#[pymodule]`:

```
def adapter(v: datetime) -> datetime: ...
def adapter_list(v: SupportsGetItem[int, datetime | None]) -> list[datetime | None]: ...
def adapter_opt(v: datetime | None = None) -> datetime | None: ...
def adapter_without_hint(v: Incomplete) -> Incomplete: ...
```

The built-in conversions all produced `datetime`. `mypy.stubtest stubprobe --ignore-missing-stub` reported no issues. Without that flag it reported one missing stub for the inner extension module. A function-style `#[pymodule]` that added submodules at run time produced a stub holding only `__getattr__`.

## 8. Proof: napi-rs

Node 24.18.0 and 26.13.1, `napi` 3.16.3, `napi-derive` 3.7.2, `@napi-rs/cli` 3.11.1, TypeScript 7.0.2.

```
napi build --platform
node node_probe.cjs <addon dir>
PG_URL=... node node_e2e.cjs <addon dir>
tsc --noEmit --strict --types '' --target esnext --lib esnext index.d.ts
```

### The built-in `chrono` conversion

```
Rust 2024 with .123456 (1710408600123456 us) -> Date -> Rust us: 2024-03-14T09:30:00.123Z -> 1710408600123000
Rust -1 us (-1 us) -> Date -> Rust us: 1969-12-31T23:59:59.999Z -> -1000
new Date(NaN) -> Rust us: 0n
new Date(NaN) echo: 1970-01-01T00:00:00.000Z
i64 return of 253402300799999999n: 253402300800000000
chrono object field retired: null: THROWS DateExpected
```

- Microseconds are lost, rounded toward the past.
- An invalid `Date` becomes 1970-01-01 with no error. [`date.rs`](https://docs.rs/crate/napi/3.16.3/source/src/bindgen_runtime/js_values/date.rs) line 146 casts `NaN` to `i64`, which gives 0.
- A Rust `i64` returns as a JavaScript `number`, so a microsecond count is not exact after year 2255.
- An `Option` field of a `#[napi(object)]` rejects `null` unless the struct sets `use_nullable = true`. This applies to adapters too.

### Hand-written conversions

All eight `bigint` adapters (`jiff` 1, `jiff` 0.2, `time`, `chrono`, `SystemTime`, `hifitime`, `temporal_rs`, `i64`) passed 3009 of 3009 instants from year 1 to 9999, with `Option` and arrays holding `null`. An RFC 3339 string adapter over `jiff` 1 passed the same 3009. It keeps nanoseconds, requires an offset and rejects `infinity`. These adapters count in `i64`, so the Postgres maximum is refused. An `i128` count would remove that limit.

End to end, `jiff` 1 newtype, identical on both Node versions:

```
scalar timestamptz: true
timestamptz[] with NULL: 2006 elements, 2006 exact
null scalar: undefined
postgres text: 2024-03-14 09:30:00.123456 {"1969-12-31 23:59:59.999999"}
```

Node 26 [enables `Temporal` by default](https://github.com/nodejs/node/blob/main/doc/changelogs/CHANGELOG_V26.md). Node 24.18.0 reports `typeof Temporal` as `undefined`. napi 3.16.3 has no `Temporal` conversion, so the probe called `Temporal.Instant.fromEpochNanoseconds` from Rust and read `epochNanoseconds` back. It accepts any object with that property and does not check the class. On Node 26:

```
exact round trips: 3009/3009
nanoseconds: 2024-03-14T09:30:00.123456789Z
list with null: [ '1970-01-01T00:00:00.000001Z', 'null' ]
past jiff MAX: THROWS InvalidArg
```

### `.d.ts`

The generator names types from the Rust identifier. It maps `DateTime`, `NaiveDateTime` and `Date` to `Date` ([`typegen.rs`](https://docs.rs/crate/napi-derive-backend/8.0.0/source/src/typegen.rs), line 533) and ignores the newtype's `TypeName`. With hints the output was `bigint`, `Array<bigint | null>` and `Temporal.Instant`. Without them:

```
export interface UnhintedRow {
  at: Ts
  retired?: Ts
}
index.d.ts(111,7): error TS2304: Cannot find name 'Ts'.
index.d.ts(112,13): error TS2304: Cannot find name 'Ts'.
```

## 9. Proof: schemars

`schemars` 1.2.3. The full shape had a required time, an optional time and a list of nullable times. The `schema_with` and built-in `SystemTime` shapes had no list, and the `hifitime` serde shape had only the required field. Each value that serialized was checked with `jsonschema` 0.63.2 with format assertions on and read back. Each shape also went through `rmcp` 3.12.0's `schema_for_type`, which returned the same properties.

| Route | Schema for the required field | Result |
|---|---|---|
| `jiff` 1 newtype | `string`, `date-time` | Pass |
| `jiff` 1 field with `schemars(with = "jiff02::Timestamp")` | `string`, `date-time` | Pass. Needs `jiff` 0.2 for the schema only |
| `jiff` 1 field with `schemars(schema_with = ...)` | `string`, `date-time` | Passes, but the optional field becomes required in the schema |
| `jiff` 1 field with `schemars(with = "String")` | `string` | `format` is lost |
| `serde_with` `DisplayFromStr` | `string` | `format` is lost |
| `jiff` 0.2 and `chrono` built in | `string`, `date-time` | Pass |
| `time` with `serde(with = "time::serde::rfc3339")` and a `schemars(with)` marker | `string`, `date-time` | Pass. A list needs a newtype or `serde_with`. With `rfc3339::option` alone, a missing field is a deserialization error |
| `SystemTime` built in | Object with `secs_since_epoch`, `nanos_since_epoch` | Before 1970: `SystemTime must be later than UNIX_EPOCH` |
| `hifitime` serde with `schemars(with = "Value")` | `true` (anything) | Writes `2024-03-14T09:30:00.123456000 UTC`, not RFC 3339 |
| `SystemTime`, `hifitime`, `temporal_rs`, `i64` newtypes writing through `jiff` 1 | `string`, `date-time` | Pass, before 1970 too |

Output for the `jiff` 1 newtype:

```
required: ["at","history"]
properties: {"at":{"format":"date-time","type":"string"},"history":{"items":{"format":"date-time","type":["string","null"]},"type":"array"},"retired":{"format":"date-time","type":["string","null"]}}
json: {"at":"2024-03-14T09:30:00.123456Z","history":["1969-07-21T02:56:14.999999Z",null],"retired":null}
valid under own schema: true []
deserializes equal: true
```

`jiff` writes a negative year as `-009999-01-02T01:59:59Z`. It reads back, but it is not valid under `format: date-time`.

## 10. Precision below a microsecond

All round trips above kept microseconds. Digits below a microsecond, which only Rust code can create here, are rounded differently by the two sides.

| Conversion | Rule | `1969-07-21T02:56:14.876543211Z` becomes |
|---|---|---|
| Postgres built-ins and the newtypes | Truncate the count from 2000-01-01 toward zero, so earlier times move later | `.876544` |
| PyO3 `jiff` 0.2 and the newtype, napi newtypes | Round toward the past | `.876543` |
| napi `chrono` | Round toward the past, to milliseconds | `.876` (from the rule, run on other values) |

The `.876544` figure is from the PR 1405 run. The released conversions and the newtype showed the same rule on another pre-2000 value. A value read from Postgres has no extra digits and is not affected.

## 11. The whole stack and the toolchain

A library with `tokio-postgres` 0.7.18, PyO3 0.30.1, napi 3.16.3, `schemars` 1.2.3, `rmcp` 3.12.0 without default features and the `jiff` 1 adapter with every feature:

```
cargo tree -i jiff      jiff v1.0.3, used by the adapter and rmcp
cargo tree -i chrono    package ID specification `chrono` did not match any packages
jiff features           alloc,serde,std
cargo deny check        advisories ok, bans ok, licenses ok, sources ok
```

`rmcp` enables `chrono` in its [default features](https://docs.rs/crate/rmcp/3.12.0/source/Cargo.toml) and depends on `jiff` 1 unconditionally.

No MSRV conflict appeared. All probes resolved the latest versions under `rust-version = "1.93.1"`. The highest declared MSRV among the direct dependencies is 1.88 (`time`, `napi`, `rmcp`, `serde_with`).

## What this means for the branch

These are facts that bear on ADR-0001 and the binding tickets. They are not a decision.

- PyO3's `jiff-02` feature [depends on `jiff` with default features](https://docs.rs/crate/pyo3/0.30.1/source/Cargo.toml). `cargo tree` showed `tz-system`, `tzdb-bundle-platform` and `tzdb-zoneinfo` enabled. ADR-0001 says gnomon enables no time zone database feature of `jiff`. That stops being true once the Python binding turns the feature on.
- The built-in `jiff` 0.2 Python conversion has the `ZoneInfo` and custom `tzinfo` limits in section 7. The direct newtype has neither.
- PyO3's unreleased `jiff` 1 conversion does not remove the first two points. Its `main` branch [declares `jiff` 1.0 with default features](https://github.com/PyO3/pyo3/blob/main/Cargo.toml), and in section 7 it gave the same 6 hour difference for the constructed `ZoneInfo`.
- Node needs a hand-written conversion and a choice of representation (`bigint`, string, or `Temporal.Instant` on Node 26) with any candidate.
- Using the (b) work waits on releases from three projects, with no dates found, and Node still needs an adapter. Moving to `jiff` 1 today costs the code in section 5.

## What remains unverified

- Only Linux x86_64 ran. `SystemTime` range differs by platform.
- Only CPython 3.14.10 ran, plus 3.10.22 for the `abi3` wheel. No free-threaded build was tried.
- Only Node 24.18.0 and 26.13.1 ran. Deno, Bun, worker threads and WebAssembly builds were not tried.
- No pooler was in the path, and no composite type or domain holding a time was tested.
- gnomon itself was not ported to `jiff` 1, and its test suite was not run. All proofs are throwaway crates.
- The adapters are proof code. They were not reviewed, benchmarked or fuzzed. All builds were debug builds except one run of the limit and panic cases.
- The `schemars` pull requests for `time` were not built.
- The forks' source was not audited. Their long-term maintenance cannot be judged after a few days.
- A search cannot prove that no adapter crate exists. Crates under unrelated names, and the reverse dependencies of `time`, were not checked.
- The `ZoneInfo` disagreement was forced with a constructed zone. No difference between real database versions was reproduced.
- No release date for PyO3 0.31, or for `jiff` 1 support in `tokio-postgres` or `schemars`, was found.

## Reproducing

The probe projects lived in a temporary directory and are not in the repository. The sections above give the commands, and the appendix gives the adapter. The test programs themselves are not reproduced here. Crate, npm and Python packages passed through the usual Cargo, npm and uv caches. The Postgres container was removed afterwards.

## Appendix: the `jiff` 1 adapter that ran

Features `postgres`, `python`, `stubs`, `node` and `schema` of one library. Dependencies: `jiff = "1"` (with `jiff/serde` under `schema`), `postgres-types` 0.2.14, `postgres-protocol` 0.6.12, `bytes`, `pyo3` 0.30.1, `napi` 3.16.3 with `napi6`, `schemars` 1.2.3 and `serde`.

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ts(pub jiff::Timestamp);

#[cfg(feature = "postgres")]
mod pg {
    use super::Ts;
    use bytes::BytesMut;
    use postgres_protocol::types;
    use postgres_types::{FromSql, IsNull, ToSql, Type, accepts, to_sql_checked};
    use std::error::Error;

    /// Microseconds from the Unix epoch to 2000-01-01 00:00:00 UTC.
    const PG_EPOCH_US: i64 = 946_684_800_000_000;

    impl<'a> FromSql<'a> for Ts {
        fn from_sql(_: &Type, raw: &'a [u8]) -> Result<Ts, Box<dyn Error + Sync + Send>> {
            let v = types::timestamp_from_sql(raw)?;
            v.checked_add(PG_EPOCH_US)
                .and_then(|us| jiff::Timestamp::from_microsecond(us).ok())
                .map(Ts)
                .ok_or_else(|| "value too large to decode".into())
        }
        accepts!(TIMESTAMPTZ);
    }

    impl ToSql for Ts {
        fn to_sql(&self, _: &Type, w: &mut BytesMut) -> Result<IsNull, Box<dyn Error + Sync + Send>> {
            // Truncate the count from 2000-01-01 toward zero, as the built-in
            // jiff 0.2 conversion does.
            let ns = self.0.as_nanosecond() - i128::from(PG_EPOCH_US) * 1_000;
            types::timestamp_to_sql(i64::try_from(ns / 1_000)?, w);
            Ok(IsNull::No)
        }
        accepts!(TIMESTAMPTZ);
        to_sql_checked!();
    }
}

#[cfg(feature = "python")]
mod py {
    use super::Ts;
    #[cfg(feature = "stubs")]
    use pyo3::PyTypeInfo;
    use pyo3::exceptions::{PyOverflowError, PyTypeError, PyValueError};
    #[cfg(feature = "stubs")]
    use pyo3::inspect::PyStaticExpr;
    use pyo3::prelude::*;
    use pyo3::types::{PyDateTime, PyDelta, PyDeltaAccess, PyTzInfo, PyTzInfoAccess};

    const DAY_US: i64 = 86_400 * 1_000_000;

    fn epoch(py: Python<'_>) -> PyResult<Bound<'_, PyDateTime>> {
        let utc = PyTzInfo::utc(py)?;
        PyDateTime::new(py, 1970, 1, 1, 0, 0, 0, 0, Some(&utc))
    }

    impl<'a, 'py> FromPyObject<'a, 'py> for Ts {
        type Error = PyErr;
        #[cfg(feature = "stubs")]
        const INPUT_TYPE: PyStaticExpr = PyDateTime::TYPE_HINT;

        fn extract(ob: Borrowed<'a, 'py, PyAny>) -> PyResult<Ts> {
            let dt = ob.cast::<PyDateTime>()?;
            // An aware datetime is one whose utcoffset() is not None. This
            // holds for fixed offsets, zoneinfo and any other tzinfo.
            if dt.get_tzinfo().is_none() || dt.call_method0("utcoffset")?.is_none() {
                return Err(PyTypeError::new_err("expected a timezone-aware datetime"));
            }
            let delta = dt.sub(epoch(ob.py())?)?;
            let delta = delta.cast::<PyDelta>()?;
            let us = i64::from(delta.get_days()) * DAY_US
                + i64::from(delta.get_seconds()) * 1_000_000
                + i64::from(delta.get_microseconds());
            jiff::Timestamp::from_microsecond(us)
                .map(Ts)
                .map_err(|e| PyValueError::new_err(e.to_string()))
        }
    }

    impl<'py> IntoPyObject<'py> for Ts {
        type Target = PyDateTime;
        type Output = Bound<'py, PyDateTime>;
        type Error = PyErr;
        #[cfg(feature = "stubs")]
        const OUTPUT_TYPE: PyStaticExpr = PyDateTime::TYPE_HINT;

        fn into_pyobject(self, py: Python<'py>) -> PyResult<Self::Output> {
            // Round toward negative infinity, as PyO3's jiff 0.2 conversion does.
            let us = i64::try_from(self.0.as_nanosecond().div_euclid(1_000))
                .map_err(|_| PyOverflowError::new_err("time out of range"))?;
            let delta = PyDelta::new(
                py,
                i32::try_from(us.div_euclid(DAY_US))?,
                i32::try_from(us.rem_euclid(DAY_US) / 1_000_000)?,
                i32::try_from(us.rem_euclid(1_000_000))?,
                false,
            )?;
            Ok(epoch(py)?.add(delta)?.cast_into::<PyDateTime>()?)
        }
    }

    impl<'py> IntoPyObject<'py> for &Ts {
        type Target = PyDateTime;
        type Output = Bound<'py, PyDateTime>;
        type Error = PyErr;
        #[cfg(feature = "stubs")]
        const OUTPUT_TYPE: PyStaticExpr = PyDateTime::TYPE_HINT;

        fn into_pyobject(self, py: Python<'py>) -> PyResult<Self::Output> {
            (*self).into_pyobject(py)
        }
    }
}

/// To JavaScript: a `bigint` of microseconds since the Unix epoch.
#[cfg(feature = "node")]
mod node {
    use super::Ts;
    use napi::bindgen_prelude::{BigInt, FromNapiValue, ToNapiValue, TypeName, ValidateNapiValue};
    use napi::{Error, Result, Status, ValueType, sys};

    impl TypeName for Ts {
        fn type_name() -> &'static str {
            "bigint"
        }
        fn value_type() -> ValueType {
            ValueType::BigInt
        }
    }

    impl ValidateNapiValue for Ts {}

    impl FromNapiValue for Ts {
        unsafe fn from_napi_value(env: sys::napi_env, value: sys::napi_value) -> Result<Self> {
            // SAFETY: napi passes a live env and value.
            let big = unsafe { BigInt::from_napi_value(env, value) }?;
            let (us, lossless) = big.get_i64();
            if !lossless {
                return Err(Error::new(Status::InvalidArg, "time out of range"));
            }
            jiff::Timestamp::from_microsecond(us)
                .map(Ts)
                .map_err(|e| Error::new(Status::InvalidArg, e.to_string()))
        }
    }

    impl ToNapiValue for Ts {
        unsafe fn to_napi_value(env: sys::napi_env, val: Self) -> Result<sys::napi_value> {
            // Round toward negative infinity, as the PyO3 conversion does.
            let us = val.0.as_nanosecond().div_euclid(1_000) as i64;
            // SAFETY: napi passes a live env.
            unsafe { BigInt::to_napi_value(env, BigInt::from(us)) }
        }
    }
}

#[cfg(feature = "schema")]
mod schema {
    use super::Ts;
    use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use std::borrow::Cow;

    impl Serialize for Ts {
        fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
            self.0.serialize(s)
        }
    }
    impl<'de> Deserialize<'de> for Ts {
        fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
            jiff::Timestamp::deserialize(d).map(Ts)
        }
    }
    impl JsonSchema for Ts {
        fn inline_schema() -> bool {
            true
        }
        fn schema_name() -> Cow<'static, str> {
            "Timestamp".into()
        }
        fn schema_id() -> Cow<'static, str> {
            "gnomon::Ts".into()
        }
        fn json_schema(_: &mut SchemaGenerator) -> Schema {
            json_schema!({ "type": "string", "format": "date-time" })
        }
    }
}
```
