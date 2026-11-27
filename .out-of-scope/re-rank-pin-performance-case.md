# A performance case for the re-rank pin

The performance suite will not get a timed case that fails when the vector Search re-rank loses its primary-key pin.

## Why this is out of scope

Search keeps the 400 nearest candidates by bit distance, then re-ranks them with their full vectors. Under row-level security the planner can replace the 400 primary-key probes with a join that reads every visible full vector (prototype 5: 660 ms against 96 ms, ADR-0010). A lateral subquery with `limit 1` pins the plan.

The plan test in the ordinary test suite already guards the pin. It runs one Search and reads PostgreSQL's counters for the full-vector table: no sequential scan, 400 index scans and 400 rows fetched, also with nested loops discouraged. It asserts no time, so it gives the same result on every run. #89 keeps it permanently, and it runs in CI once #88 lands.

A timed case would add one thing, evidence that the pin is still needed to meet the Search budget. That is not worth its cost:

- The performance suite runs only on demand, on a quiet machine.
- No data set has reproduced the slow plan. With the pin removed, the 200,000-row case measured 179.5 ms at p95, against 183.8 ms with the pin and a budget of 250 ms. Calibrating new data may not succeed.

## When to reconsider

- The plan test is removed, or can no longer tell the pinned plan from the unpinned one.
- A re-rank is added that no plan test covers.
- A measurement shows the unpinned plan breaking a budget on data the suite can build.

The other half of the same issue, a Search query holding a common word, was not rejected. #85 adds those cases and #87 tracks their cost.

## Prior requests

- #79: "The performance suite misses the re-rank pin and a common-word query"
