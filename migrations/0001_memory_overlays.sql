-- donto-memory consumer overlays.
--
-- Five tables, all under the `donto_x_memory_*` prefix per the
-- substrate's M10 §6.1 overlay-naming convention:
--
--   donto_x_memory_module                — registered modules.
--   donto_x_memory_record                — one row per unit of memory.
--   donto_x_memory_access                — append-only access events.
--   donto_x_memory_state                 — derived recall state per record.
--   donto_x_memory_reconsolidation_queue — sleep-path work items.
--
-- Substrate-contract notes:
--
--  * Every overlay table that mutates over time carries `tx_time
--    tstzrange + lower_inc check` per M10 §6.1 lint requirement.
--  * Every overlay table FK's to substrate core (donto_context for
--    the lightweight ones; donto_statement / donto_document where
--    direct anchoring is appropriate).
--  * No `ON DELETE CASCADE` against substrate tables — if a
--    statement is retracted (closes its tx_time, but the row stays),
--    the memory record remains queryable for audit.
--  * After applying this file, run `donto overlay register` for
--    each table so the substrate registry knows about them.
--
-- This file is idempotent: re-running is a no-op.

-- ---------------------------------------------------------------------------
-- 0. Ensure the donto-memory root context exists so subsequent FKs
--    from overlay tables to donto_context(iri) resolve.
--    Idempotent via on conflict.
-- ---------------------------------------------------------------------------

select donto_ensure_context('ctx:memory', 'custom', 'permissive', null);

-- ---------------------------------------------------------------------------
-- 1. donto_x_memory_module — module registry.
--    Carries tx_time so the substrate M10 §6.1 lint accepts it as a
--    bitemporal overlay. FK to donto_context(iri) via consumer_iri
--    anchors it to substrate core. Enable/disable still uses the
--    boolean column (cheaper than tx_time close-and-reopen).
-- ---------------------------------------------------------------------------

create table if not exists donto_x_memory_module (
    module_iri    text primary key,
    consumer_iri  text not null default 'ctx:memory'
                  references donto_context(iri),
    form          text not null check (form in (
                      'token', 'structured', 'parametric', 'dream'
                  )),
    function      text not null check (function in (
                      'factual', 'experiential', 'procedural',
                      'preference', 'working'
                  )),
    version       text not null default 'v0.1.0',
    label         text,
    description   text,
    config        jsonb not null default '{}'::jsonb,
    enabled       boolean not null default true,
    created_at    timestamptz not null default now(),
    modified_at   timestamptz not null default now(),
    tx_time       tstzrange not null default tstzrange(now(), null, '[)'),
    constraint donto_x_memory_module_tx_lower_inc check (lower_inc(tx_time))
);

create index if not exists donto_x_memory_module_consumer_idx
    on donto_x_memory_module (consumer_iri);
create index if not exists donto_x_memory_module_form_idx
    on donto_x_memory_module (form, function) where enabled;

-- ---------------------------------------------------------------------------
-- 2. donto_x_memory_record — one row per unit of memory.
--    Each record is anchored to a substrate primary key — either a
--    statement (for atomic memories), a frame (for compound),
--    or a context (when the whole context IS the memory unit, e.g.
--    an episodic chunk).
-- ---------------------------------------------------------------------------

create table if not exists donto_x_memory_record (
    record_id      uuid primary key default gen_random_uuid(),
    record_iri     text unique not null,
    module_iri     text not null
                   references donto_x_memory_module(module_iri),
    -- One of these three must be set; substrate FK ensures the
    -- anchor exists.
    root_statement uuid references donto_statement(statement_id),
    root_frame     uuid references donto_claim_frame(frame_id),
    root_context   text references donto_context(iri),
    -- Session/holder/policy metadata. Holder is the agent the
    -- memory belongs to; session_iri scopes a multi-turn dialogue.
    holder_iri     text,
    session_iri    text,
    -- M10 §6.7: the policy capsule the consumer expects to govern
    -- this record. The substrate's policy assignment on the root
    -- target is the source of truth; this is a cache for audit.
    expected_policy_iri text,
    -- Bitemporal: tx_time tracks when this record was *created in
    -- the memory layer* (separate from substrate belief).
    tx_time        tstzrange not null default tstzrange(now(), null, '[)'),
    metadata       jsonb not null default '{}'::jsonb,
    constraint donto_x_memory_record_one_anchor check (
        (root_statement is not null)::int +
        (root_frame is not null)::int +
        (root_context is not null)::int = 1
    ),
    constraint donto_x_memory_record_tx_lower_inc check (lower_inc(tx_time))
);

create index if not exists donto_x_memory_record_module_idx
    on donto_x_memory_record (module_iri);
create index if not exists donto_x_memory_record_holder_idx
    on donto_x_memory_record (holder_iri) where holder_iri is not null;
create index if not exists donto_x_memory_record_session_idx
    on donto_x_memory_record (session_iri) where session_iri is not null;
create index if not exists donto_x_memory_record_tx_idx
    on donto_x_memory_record using gist (tx_time);

-- ---------------------------------------------------------------------------
-- 3. donto_x_memory_access — append-only access events.
--    NOT bitemporal in the substrate sense — every row is a
--    timestamped event, never updated, never retracted. The
--    "tx_time" of an access is the access timestamp; we use a
--    range so the M10 §6.1 lint passes uniformly.
-- ---------------------------------------------------------------------------

create table if not exists donto_x_memory_access (
    access_id    uuid primary key default gen_random_uuid(),
    record_id    uuid not null references donto_x_memory_record(record_id),
    -- consumer_iri denormalizes the donto-memory deployment a row
    -- belongs to. FK to donto_context satisfies the substrate's
    -- M10 §6.1 lint requirement that every overlay table reference
    -- at least one substrate (non-overlay) primary key.
    consumer_iri text not null default 'ctx:memory'
                 references donto_context(iri),
    actor_iri    text not null,
    query_hash   text,
    access_kind  text not null check (access_kind in (
                     'retrieved', 'surfaced', 'cited',
                     'ignored', 'corrected'
                 )),
    rank         int,
    score        double precision,
    tx_time      tstzrange not null default tstzrange(now(), null, '[)'),
    constraint donto_x_memory_access_tx_lower_inc check (lower_inc(tx_time))
);

create index if not exists donto_x_memory_access_record_idx
    on donto_x_memory_access (record_id);
create index if not exists donto_x_memory_access_actor_idx
    on donto_x_memory_access (actor_iri);
create index if not exists donto_x_memory_access_time_idx
    on donto_x_memory_access (lower(tx_time) desc);

-- ---------------------------------------------------------------------------
-- 4. donto_x_memory_state — derived recall state per record.
--    Bitemporal: state changes over time (salience decays, recall
--    counts grow). Each state row is a snapshot; the *current* state
--    is the row with `upper_inf(tx_time)`.
-- ---------------------------------------------------------------------------

create table if not exists donto_x_memory_state (
    state_id          uuid primary key default gen_random_uuid(),
    record_id         uuid not null references donto_x_memory_record(record_id),
    consumer_iri      text not null default 'ctx:memory'
                      references donto_context(iri),
    salience          double precision not null default 0,
    recall_count      bigint not null default 0,
    last_accessed_at  timestamptz,
    last_modified_at  timestamptz,
    consolidated_at   timestamptz,
    next_review_at    timestamptz,
    decay_clock       interval,
    tx_time           tstzrange not null default tstzrange(now(), null, '[)'),
    constraint donto_x_memory_state_tx_lower_inc check (lower_inc(tx_time))
);

create index if not exists donto_x_memory_state_record_open_idx
    on donto_x_memory_state (record_id)
    where upper(tx_time) is null;
create index if not exists donto_x_memory_state_review_idx
    on donto_x_memory_state (next_review_at)
    where upper(tx_time) is null and next_review_at is not null;
create index if not exists donto_x_memory_state_salience_idx
    on donto_x_memory_state (salience desc)
    where upper(tx_time) is null;

-- ---------------------------------------------------------------------------
-- 5. donto_x_memory_reconsolidation_queue — sleep-path work items.
--    The sleep-path worker pulls available_at <= now() items,
--    claims them by setting claimed_at + claimed_by, and writes
--    completed_at when done. Idempotent re-queue uses the same
--    composite key (record_id, reason) within a configurable
--    coalesce window.
-- ---------------------------------------------------------------------------

create table if not exists donto_x_memory_reconsolidation_queue (
    queue_id      uuid primary key default gen_random_uuid(),
    record_id     uuid not null references donto_x_memory_record(record_id),
    consumer_iri  text not null default 'ctx:memory'
                  references donto_context(iri),
    reason        text not null check (reason in (
                      'recall', 'contradiction', 'policy_change',
                      'scheduled_review', 'explicit'
                  )),
    priority      double precision not null default 0,
    available_at  timestamptz not null default now(),
    claimed_at    timestamptz,
    claimed_by    text,
    completed_at  timestamptz,
    payload       jsonb not null default '{}'::jsonb,
    tx_time       tstzrange not null default tstzrange(now(), null, '[)'),
    constraint donto_x_memory_reconsolidation_queue_tx_lower_inc
        check (lower_inc(tx_time))
);

create index if not exists donto_x_memory_reconsol_available_idx
    on donto_x_memory_reconsolidation_queue (available_at)
    where completed_at is null and claimed_at is null;
create index if not exists donto_x_memory_reconsol_claimed_idx
    on donto_x_memory_reconsolidation_queue (claimed_by, claimed_at)
    where claimed_at is not null and completed_at is null;
create index if not exists donto_x_memory_reconsol_record_idx
    on donto_x_memory_reconsolidation_queue (record_id);

-- ---------------------------------------------------------------------------
-- 6. Seed the three default modules: episodic / semantic-claim /
--    preference. Consumers may add more via the API.
-- ---------------------------------------------------------------------------

insert into donto_x_memory_module
    (module_iri, form, function, label, description)
values
    ('mem:module/episodic',
     'token', 'experiential', 'Episodic',
     'Verbatim event/chunk recall — the raw user-utterance store.'),
    ('mem:module/semantic-claim',
     'structured', 'factual', 'Semantic Claim',
     'Extracted typed claims with subject/predicate/object/anchor.'),
    ('mem:module/preference',
     'structured', 'preference', 'Preference',
     'User preferences that never silently overwrite — every update is an event.')
on conflict (module_iri) do nothing;

-- ---------------------------------------------------------------------------
-- 7. Helper view: open state per record (the "current state").
-- ---------------------------------------------------------------------------

create or replace view donto_x_memory_current_state as
    select s.*
      from donto_x_memory_state s
     where upper(s.tx_time) is null;
