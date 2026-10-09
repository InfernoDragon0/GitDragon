# Commit graph benchmark

Targets (design "Performance & app shell"): **repo open < 500 ms**, **graph 60 fps on 1M commits**.
In practice: first rows on screen well under 500 ms, a viewport query far below one 16 ms frame.

## How to measure

```sh
cargo build -p git-core --release --example graph_bench
target/release/examples/graph_bench <repo> [--runs 3] [--skip-cold]
```

`crates/git-core/examples/graph_bench.rs` runs the graph view's feed (`log_stream(LogQuery::graph())`,
`git log --all --topo-order` in 500-commit batches -> `History::extend`) without the UI.

| Line | Meaning |
|---|---|
| `no commit-graph` | cold first open: `core.commitGraph=false` set in the repo for this run, unset afterwards (bench clones only) |
| `ensure_commit_graph` | what the app runs 2 s after open: `commit-graph write --reachable --split --changed-paths` |
| `commit-graph run N` | the same load with the commit-graph file |
| `first rows` | first batch laid out, i.e. the first screen of rows can paint |
| `full load` | the whole history laid out |
| `viewport` | lane spans + row data for a 60-row window, averaged over ~1000 positions |
| `ws` / `peak working set` | bench process memory (git's own process not counted) |

Note the machine load with each result (other apps open, power plan): `--topo-order` without a
commit-graph is disk- and CPU-bound, so it moves with whatever else is running.

## Results

<!-- ### YYYY-MM-DD · <repo> (<commits> commits, <refs>) · <git version> · <machine / load> -->

### 2026-10-09 · linux (1,484,916 commits, 952 refs) · git 2.40.1.windows.1 · Ryzen 7 5700X, 32 GB, Unreal Editor open (5.3 GB, 3.4 GB RAM free)

Release build, fresh clone (no commit-graph until `ensure_commit_graph`).

| Run | First rows | Full load | per commit | Working set |
|---|---|---|---|---|
| no commit-graph (cold open) | 9.6 s | 16.0 s | 10.8 us | 251 MiB |
| commit-graph, run 1 | 1.2 s | 15.4 s | 10.4 us | 253 MiB |
| commit-graph, run 2 | 1.3 s | 15.5 s | 10.5 us | 252 MiB |
| commit-graph, run 3 | 1.2 s | 15.8 s | 10.7 us | 252 MiB |

- `ensure_commit_graph`: **Written in 121 s** (`--reachable --split --changed-paths`, background, 2 s after open).
- Viewport (60 rows): **3.1 us** avg over 1001 queries, 295 spans per window, lane width **804**. Peak working set 261 MiB.

Where the time goes (raw `git log --all --topo-order -z`, output to NUL, commit-graph present):

| Format | Time |
|---|---|
| `-n 1 --format=%H` (first line, topo order) | 1.17 s |
| `-n 1 --format=%H` (first line, date order) | 0.08 s |
| `%H %P` | 3.4 s |
| `%H %P %D` | 3.75 s |
| `%H %P %s` | 12.6 s |
| `%H %P %an %ae %ad` | 13.1 s |
| full `GRAPH_FORMAT` | 14.0 s |

- First rows are git's own `--topo-order --all` latency (1.17 s of 1.2 s): misses the 500 ms target.
- Full load is ~90 % git: any field from the commit body (subject, author) makes git inflate every
  commit object (+9 s); parents and refs come from the commit-graph alone. Our parse + layout is ~1.5 s.
- Viewport cost is negligible against a 16 ms frame; 295 spans per window still needs a hands-on
  60 fps check in the real view.

### 2026-10-09 · linux, after fixes · same machine, Unreal Editor still open

- **Date-order preview** (first load shows 200 rows in date order until the topo-ordered graph's
  first batch replaces them): **~90 ms** with a commit-graph (3 runs: 96 / 80 / 90 ms), 857 ms on a
  fresh clone before the graph exists. The topo-ordered first rows stay at 1.2 s, now behind the preview.
- **`ensure_commit_graph` without changed-path Bloom filters** (packs > 1 GiB): **13.2 s**, was 121 s.
- Full load unchanged (15.1 s, git-bound); viewport 2.0 us; peak working set 261 MiB.
- Fresh-clone topo first rows measured 14.5 s this run (9.6 s before): disk/CPU contention with
  the editor; the preview covers it either way.

### 2026-10-09 · linux, Unreal Editor closed (baseline) · same build as the run above

| Run | First rows | Full load | Working set |
|---|---|---|---|
| date-order preview (fresh, no commit-graph) | 0.96 s (200 rows) | | |
| no commit-graph (cold open) | 15.0 s | 21.7 s | 252 MiB |
| commit-graph, run 1 / 2 / 3 | 1.2 / 1.3 / 1.3 s | 16.8 / 17.1 / 16.6 s | 253 MiB |

- `ensure_commit_graph` (no Bloom filters): 14.1 s. Viewport 1.9 us. Peak working set 261 MiB.
- No faster than with the editor open (full load ~1.5 s slower, within noise): the editor sat idle,
  and source files were being edited during this run, so rust-analyzer may have been busy. The load
  is git-bound either way; an idle editor costs GitDragon nothing measurable.
