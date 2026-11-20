## 100k-message store budgets

| Operation | Samples | p50 (ms) | p95 (ms) or RSS (MB) | Baseline | Limit (+20%) | Result |
| --- | ---: | ---: | ---: | ---: | ---: | --- |
| RSS after store open | 1 | — | 39.404 | 18.000 | 21.600 | **Fail** |
| RSS after first Inbox page | 1 | — | 39.961 | 18.000 | 21.600 | **Fail** |
| RSS after opening 20 distinct threads | 1 | — | 40.944 | 18.000 | 21.600 | **Fail** |
| Open populated store | 31 | 1.272 | 1.506 | — | — | Informational |
| Inbox first page (100 rows) | 31 | 0.350 | 0.377 | 0.750 | 0.900 | Pass |
| Inbox page near middle cursor (100 rows) | 31 | 0.390 | 0.417 | 0.750 | 0.900 | Pass |
| Read and present a 50-message thread | 31 | 1.655 | 1.810 | 3.000 | 3.600 | Pass |
| Keyword · Single term (`invoice`) | 31 | 12.674 | 13.111 | 25.000 | 30.000 | Pass |
| Keyword · Multiple terms (`boiler leak`) | 31 | 9.975 | 10.782 | 25.000 | 30.000 | Pass |
| Keyword · Quoted phrase (`"boarding pass"`) | 31 | 7.432 | 7.939 | 25.000 | 30.000 | Pass |
| Keyword · Text and operators (`warranty from:p10@bench.test in:inbox has:attachment after:2023-12-01`) | 31 | 4.253 | 5.018 | 25.000 | 30.000 | Pass |
| Keyword · Misspelling (`invioce`) | 31 | 11.915 | 12.188 | 25.000 | 30.000 | Pass |

Hybrid search skipped because the pinned local MiniLM model is absent.

Fixture generation took 236.97s (773.7 MB on disk). Full suite took 238.69s, including setup and cleanup. Each latency row uses nearest-rank percentiles. RSS values are single process snapshots in decimal MB.
