-- ============================================================================
-- donto_statement_fts_name — substrate-wide full-text "name" search index
-- ============================================================================
-- Backs POST /search (crates/donto-memory/src/api/routes/search.rs).
--
-- WHY: /recall is scoped to ctx:memory/* (~44K stmts). /search must reach the
-- whole substrate (39M+ stmts, all ctx:*). Names live in three shapes across
-- the corpus:
--   * readable IRI path segments  (ctx:genes/.../person/caroline-rose)
--   * rdfs:label literals over opaque md5 IRIs (ctx:genealogy/research-db)
--   * short labels
-- A single GIN tsvector index over a projection that replaces `/ - :` with
-- spaces (so IRI segments tokenise) on subject + object_iri, plus the first
-- 120 chars of the literal value, covers all three with real ranking
-- (ts_rank) and sub-second latency. ("humanize" below is descriptive prose,
-- NOT a SQL function — the real, executed expression is exactly the CREATE
-- INDEX body below and must stay byte-identical to FTS_EXPR in search.rs.)
--
-- NOTE: each coalesce(..., '') is MANDATORY. In Postgres `x || NULL` is NULL,
-- so to_tsvector over a concatenation with any NULL arm yields NULL — which
-- both drops rows (exactly one of object_iri/object_lit is non-null per row)
-- and de-qualifies this partial index. Do not remove the coalesces.
--
-- The 120-char literal cap keeps long episodic text out of the index (size +
-- relevance) while still indexing names/labels.
--
-- CRITICAL: the query in search.rs (FTS_EXPR constant) MUST use this EXACT
-- expression, and include `upper(tx_time) IS NULL`, or the planner will not
-- use this PARTIAL index and will seq-scan 39M rows.
--
-- Build CONCURRENTLY (no table lock) on the live DB. ~4GB. Build with a
-- raised maintenance_work_mem for speed:
--   SET maintenance_work_mem = '1GB';
-- ============================================================================

CREATE INDEX CONCURRENTLY IF NOT EXISTS donto_statement_fts_name
ON donto_statement
USING gin (
  to_tsvector('simple',
    coalesce(replace(replace(replace(subject,    '/',' '),'-',' '),':',' '), '') || ' ' ||
    coalesce(replace(replace(replace(object_iri, '/',' '),'-',' '),':',' '), '') || ' ' ||
    left(coalesce(object_lit->>'v',''), 120))
)
WHERE upper(tx_time) IS NULL;
