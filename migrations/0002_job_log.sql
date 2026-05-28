-- donto-memory job log.
--
-- Every /memorize, /memorize/batch, /recall, /ingest call writes one
-- row here capturing the full request, response, model usage, and
-- elapsed time. Powers the /jobs HTML observability page.
--
-- Idempotent: re-running is a no-op.

create table if not exists donto_x_memory_job_log (
    job_id           uuid primary key default gen_random_uuid(),
    consumer_iri     text not null default 'ctx:memory'
                     references donto_context(iri),
    created_at       timestamptz not null default now(),

    -- Endpoint label, e.g. "POST /memorize", "POST /recall",
    -- "POST /ingest/semantic-claim".
    endpoint         text not null,

    -- Caller identity, parsed out of the request body for fast filter.
    holder           text,
    session_id       text,

    status_code      int  not null,
    elapsed_ms       bigint not null,

    -- Full request + response bodies (JSON). Truncation policy is
    -- "store as-is"; if a response is huge, that is itself useful
    -- diagnostic information.
    request          jsonb not null,
    response         jsonb not null,

    -- Quick-glance metrics so the list page can sort/aggregate
    -- without scanning the response JSON.
    facts_extracted  int,
    facts_ingested   int,
    rows_returned    int,
    model            text,
    prompt_tokens    int,
    completion_tokens int,
    total_tokens     int,
    error            text,

    -- Bitemporal lower-inclusive for substrate M10 §6.1 lint parity.
    tx_time          tstzrange not null default tstzrange(now(), null, '[)'),
    constraint donto_x_memory_job_log_tx_lower_inc check (lower_inc(tx_time))
);

create index if not exists donto_x_memory_job_log_created_idx
    on donto_x_memory_job_log (created_at desc);
create index if not exists donto_x_memory_job_log_endpoint_idx
    on donto_x_memory_job_log (endpoint, created_at desc);
create index if not exists donto_x_memory_job_log_holder_idx
    on donto_x_memory_job_log (holder, created_at desc)
    where holder is not null;
create index if not exists donto_x_memory_job_log_session_idx
    on donto_x_memory_job_log (session_id, created_at desc)
    where session_id is not null;
