## 100k-message store budgets

| Operation | Samples | p50 (ms) | p95 (ms) or RSS (MB) | Baseline | Limit (+20%) | Result |
| --- | ---: | ---: | ---: | ---: | ---: | --- |
| RSS after store open | 1 | — | 39.617 | 18.000 | 21.600 | **Fail** |
| RSS after first Inbox page | 1 | — | 40.174 | 18.000 | 21.600 | **Fail** |
| RSS after opening 20 distinct threads | 1 | — | 41.140 | 18.000 | 21.600 | **Fail** |
| Open populated store | 31 | 1.788 | 3.246 | — | — | Informational |
| Inbox first page (100 rows) | 31 | 0.419 | 0.461 | 0.750 | 0.900 | Pass |
| Inbox page near middle cursor (100 rows) | 31 | 0.384 | 0.390 | 0.750 | 0.900 | Pass |
| Read and present a 50-message thread | 31 | 1.578 | 1.757 | 3.000 | 3.600 | Pass |
| Keyword · Single term (`invoice`) | 31 | 11.174 | 12.155 | 25.000 | 30.000 | Pass |
| Keyword · Multiple terms (`boiler leak`) | 31 | 8.823 | 9.726 | 25.000 | 30.000 | Pass |
| Keyword · Quoted phrase (`"boarding pass"`) | 31 | 6.396 | 7.566 | 25.000 | 30.000 | Pass |
| Keyword · Text and operators (`warranty from:p10@bench.test in:inbox has:attachment after:2023-12-01`) | 31 | 128.617 | 137.149 | 25.000 | 30.000 | **Fail** |
| Keyword · Misspelling (`invioce`) | 31 | 11.883 | 15.175 | 25.000 | 30.000 | Pass |

Hybrid search skipped because the pinned local MiniLM model is absent.

Fixture generation took 202.30s (773.7 MB on disk). Full suite took 207.96s, including setup and cleanup. Each latency row uses nearest-rank percentiles. RSS values are single process snapshots in decimal MB.
