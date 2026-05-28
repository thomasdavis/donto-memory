# donto-memory

> Agentic-memory runtime that runs on top of the
> [donto](https://github.com/thomasdavis/donto) evidence substrate.

donto-memory is a long-running service for an agent's memory,
written in Rust. It sits as a consumer of a donto substrate, treating
donto as a remote dependency: the substrate runs separately, dontosrv
exposes the HTTP contract, donto-memory binds to it over the network.

**Design centre:** read-time dynamics (salience, recall counts,
session windows) live in *consumer overlay* tables that the
substrate runs lint over. Memory content is stored as
*evidence-anchored claims under contexts* in the substrate proper.

## Status

| | |
|---|---|
| Version | 0.1.0 |
| Substrate contract floor | `0.1.0-m10` |
| Rust | edition 2021, rust-version 1.78+ |
| Substrate URL | configurable via `DONTO_MEMORY_DONTOSRV_URL` |

## Three commitments donto-memory makes

1. **No silent rewrite.** Reconsolidation derives new claims from
   recalled ones — it never overwrites the originals. The original
   chunks remain queryable forever; derived summaries are
   *additional* claims with modality `derived` and a `supersedes`
   argument edge back to the source.
2. **Read events are not belief events.** A recall *bumps a counter*
   in `donto_x_memory_access` (overlay), not anywhere in the
   substrate's `donto_statement` table. The substrate's bitemporal
   discipline is preserved: `tx_time` reflects belief, not access.
3. **Policy-aware by default.** Every recall passes a holder agent
   and an action through to `POST /recall` on the substrate. The
   substrate returns rows the holder is attested for; donto-memory
   passes them through unchanged.

## Architecture

```
       ┌──────────────────────────────────────────────┐
       │  donto-memory (single binary)                │
       │                                              │
       │   donto-memory api      donto-memory worker  │
       │   (axum :7900)          (tokio loop)         │
       │           │                     │            │
       │           ▼                     ▼            │
       │   ┌───────────────────────────────────────┐  │
       │   │   donto_memory_core (library)         │  │
       │   │   - modules: episodic / semantic /    │  │
       │   │     preference (+ MemoryModule trait) │  │
       │   │   - hot_path: recall composer + RRF   │  │
       │   │   - sleep_path: reflect + apply       │  │
       │   │   - substrate: reqwest → dontosrv     │  │
       │   │   - overlays: tokio-postgres helpers  │  │
       │   └────────────────────┬──────────────────┘  │
       └────────────────────────┼─────────────────────┘
                                ▼
                    ┌────────────────────────┐
                    │   donto (substrate)    │
                    │   dontosrv :7879       │
                    │   (any donto instance) │
                    └────────────────────────┘
```

donto-memory ships **one binary** (`donto-memory`) with three
subcommands:

- `donto-memory migrate` — apply overlay migrations + register
  overlays with the substrate.
- `donto-memory api` — run the axum HTTP server.
- `donto-memory worker` — run the sleep-path reconsolidation worker.

Two long-running deployments (`donto-memory api` + `donto-memory
worker`) share five overlay tables in the substrate's Postgres,
all under the `donto_x_memory_*` prefix the substrate's M10 §6.1
lint enforces.

## Memory modules

A memory module is a plugin defining a particular memory form +
function. Three ship with the runtime:

| Module IRI | Form | Function | Notes |
|---|---|---|---|
| `mem:module/episodic` | token | experiential | Verbatim event/chunk recall — the raw user-utterance store. |
| `mem:module/semantic-claim` | structured | factual | Extracted typed claims with subject/predicate/object/anchor. |
| `mem:module/preference` | structured | preference | User preferences that never silently overwrite — every update is an event. |

New modules implement the `MemoryModule` trait in
`donto_memory_core::module` and register via `MODULE_REGISTRY`. The
trait's contract is documented inline.

## Quick start

### Prerequisites

- Rust ≥ 1.78 (`rustup install stable`).
- A running donto substrate (M10+). Default
  `DONTO_MEMORY_DONTOSRV_URL=http://localhost:7879`; the live
  donto-db instance is at `https://genes.apexpots.com`.

### Build + run

```bash
git clone https://github.com/thomasdavis/donto-memory.git
cd donto-memory
cargo build --release

# 1. Apply overlay migrations + register with the substrate.
./target/release/donto-memory migrate \
    --substrate-url http://localhost:7879 \
    --dsn $DONTO_DSN

# 2. Run the API server.
./target/release/donto-memory api --bind 127.0.0.1:7900

# 3. Or run the sleep worker.
./target/release/donto-memory worker
```

### Hit the API

```bash
curl -s http://localhost:7900/health
curl -s http://localhost:7900/modules

# Ingest a chunk into the episodic module.
curl -s -X POST http://localhost:7900/ingest/episodic \
  -H 'Content-Type: application/json' \
  -d '{
    "holder": "agent:ajax",
    "session_id": "s-2026-05-28",
    "text": "I met Annie Davis at the Cooktown Festival in 1979."
  }'

# Recall — single-call Memory Evidence Bundle.
curl -s -X POST http://localhost:7900/recall \
  -H 'Content-Type: application/json' \
  -d '{
    "holder": "agent:ajax",
    "action": "read_content",
    "query": "Annie Davis",
    "session_id": "s-2026-05-28",
    "limit": 20,
    "permitted_only": true
  }'
```

## Substrate contract version

donto-memory binds to substrate contract version `0.1.0-m10`. The
runtime checks `GET /discovery/contract-version` on startup and
fails fast if the substrate is older than the pinned floor.

## Project layout

```
donto-memory/
├── Cargo.toml                 workspace
├── migrations/                consumer overlay SQL
├── crates/
│   ├── donto-memory-core/     types, substrate client, modules, hot/sleep
│   └── donto-memory/          the donto-memory binary (clap subcommands)
├── deploy/                    systemd + Dockerfile + Caddy snippet
└── tests/                     integration smoke
```

## License

Apache-2.0 OR MIT, at your option.
