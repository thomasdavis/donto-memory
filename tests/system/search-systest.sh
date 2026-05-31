#!/usr/bin/env bash
# ============================================================================
# donto substrate-wide /search (FTS) — system test harness
# ============================================================================
# Exercises POST /search end-to-end against the LIVE substrate and asserts
# behaviour: reachability, latency, result shape, cross-context reach,
# ts_rank ordering, relevance, multi-word AND, context_prefix filter, limit,
# edge cases, the partial-degradation contract, and the parity gap vs the
# holder-scoped /recall.
#
# One `PASS <name>` / `FAIL <name> :: <detail>` per assertion; final
# `RESULT pass=<n> fail=<n>`. Exit 0 iff all pass.
#
# Usage:  ./donto-search-systest.sh [BASE_URL]
#   default http://127.0.0.1:7900 ; use https://memories.apexpots.com for edge.
# ============================================================================
set -uo pipefail

BASE="${1:-http://127.0.0.1:7900}"
SEARCH="$BASE/search"
RECALL="$BASE/recall"
HOLDER="agent:omega-bot"
TMP="$(mktemp -d)"
PASS=0; FAIL=0
LAT_BUDGET_MS=8000

ok()  { echo "PASS $1"; PASS=$((PASS+1)); }
bad() { echo "FAIL $1 :: ${2:-}"; FAIL=$((FAIL+1)); }

# search QUERY [EXTRA_JSON] -> writes $TMP/last.json, echoes "HTTPCODE TIME"
search() {
  local q="$1" extra="${2:-}"
  local body="{\"query\":$(printf '%s' "$q" | jq -Rsa .)${extra:+,$extra}}"
  curl -s -o "$TMP/last.json" -w "%{http_code} %{time_total}" \
    --max-time 30 -XPOST "$SEARCH" -H 'Content-Type: application/json' -d "$body"
}
jqf() { jq -r "$1" "$TMP/last.json" 2>/dev/null; }

echo "# donto /search (FTS) system test  base=$BASE  $(date -u +%FT%TZ)"

# --- T1: reachability + 200 + shape ----------------------------------------
read -r CODE T <<<"$(search 'Brackenridge' '"limit":5')"
[ "$CODE" = 200 ] && ok t1_http_200 || bad t1_http_200 "code=$CODE"
RC=$(jqf '.row_count')
if [ "${RC:-0}" -ge 1 ] 2>/dev/null; then ok t2_returns_rows "rc=$RC"; else bad t2_returns_rows "rc=$RC"; fi
HASF=$(jqf '.rows[0] | has("subject") and has("predicate") and has("context") and has("score")')
[ "$HASF" = true ] && ok t3_row_shape || bad t3_row_shape "missing fields"

# --- T4: latency bound ------------------------------------------------------
MS=$(jqf '.elapsed_ms')
if [ "${MS:-999999}" -le "$LAT_BUDGET_MS" ] 2>/dev/null; then ok t4_latency "${MS}ms"; else bad t4_latency "${MS}ms > ${LAT_BUDGET_MS}ms"; fi

# --- T5: cross-context reach (the whole point) -----------------------------
TOPCTX=$(jqf '.rows[0].context')
case "$TOPCTX" in
  ctx:genes*|ctx:genealogy*|ctx:research*) ok t5_reaches_substrate "$TOPCTX" ;;
  *) bad t5_reaches_substrate "top ctx=$TOPCTX" ;;
esac

# --- T6: ts_rank ordering is descending ------------------------------------
SORTED=$(jqf '[.rows[].score] | . == (sort | reverse)')
[ "$SORTED" = true ] && ok t6_ranked_desc || bad t6_ranked_desc "not sorted desc"

# --- T7: relevance — top hit actually mentions the term --------------------
search 'Yeatman' '"limit":5' >/dev/null
HIT=$(jqf '.rows[0] | (.subject + " " + (.object_iri // "") + " " + ((.object_lit.v // "")|tostring)) | ascii_downcase | contains("yeatman")')
[ "$HIT" = true ] && ok t7_relevance || bad t7_relevance "top hit lacks term"

# --- T8: multi-word AND (plainto_tsquery) ----------------------------------
search 'Caroline Rose' '"limit":10' >/dev/null
CN=$(jqf '[.rows[] | (.subject+" "+(.object_iri//"")+" "+((.object_lit.v//"")|tostring)) | ascii_downcase | select(contains("caroline"))] | length')
if [ "${CN:-0}" -ge 1 ] 2>/dev/null; then ok t8_multiword "caroline_rows=$CN"; else bad t8_multiword "no caroline rows"; fi

# --- T9: context_prefix filter -> only that family -------------------------
search 'Davis' '"limit":15,"context_prefix":"ctx:genealogy"' >/dev/null
OUTSIDE=$(jqf '[.rows[] | select((.context|startswith("ctx:genealogy"))|not)] | length')
if [ "${OUTSIDE:-1}" = 0 ]; then ok t9_prefix_filter; else bad t9_prefix_filter "rows outside prefix=$OUTSIDE"; fi

# --- T10: limit is honoured -------------------------------------------------
search 'Davis' '"limit":3' >/dev/null; L3=$(jqf '.row_count')
if [ "${L3:-99}" -le 3 ] 2>/dev/null; then ok t10_limit_honoured "n=$L3"; else bad t10_limit_honoured "n=$L3 > 3"; fi

# --- T11: empty query -> 400 -----------------------------------------------
read -r CODE T <<<"$(search '   ' '"limit":5')"
[ "$CODE" = 400 ] && ok t11_empty_400 || bad t11_empty_400 "code=$CODE"

# --- T12: nonsense term -> clean 200 with 0 rows ---------------------------
read -r CODE T <<<"$(search 'zzqqxhwvkj_no_such_token_42' '"limit":5')"
RC=$(jqf '.row_count')
if [ "$CODE" = 200 ] && [ "${RC:-9}" = 0 ]; then ok t12_no_match_clean; else bad t12_no_match_clean "code=$CODE rc=$RC"; fi

# --- T13: never 500 on a hot common term (partial OR fast OK, never error) --
read -r CODE T <<<"$(search 'the' '"limit":5')"
PARTIAL=$(jqf '.partial')
if [ "$CODE" = 200 ]; then ok t13_hot_term_no_500 "code=200 partial=$PARTIAL"; else bad t13_hot_term_no_500 "code=$CODE"; fi

# --- T14: parity gap — search reaches contexts recall structurally cannot --
search 'Brackenridge' '"limit":20' >/dev/null
SUB_CTX=$(jqf '[.rows[] | select((.context|startswith("ctx:genes")) or (.context|startswith("ctx:genealogy")))] | length')
REC_SUB_CTX=$(curl -s --max-time 20 -XPOST "$RECALL" -H 'Content-Type: application/json' \
      -d "{\"holder\":\"$HOLDER\",\"query\":\"Brackenridge\",\"limit\":20,\"permitted_only\":false}" \
      | jq -r '[.rows[]? | select((.context|startswith("ctx:genes")) or (.context|startswith("ctx:genealogy")))] | length' 2>/dev/null)
if [ "${SUB_CTX:-0}" -ge 1 ] && [ "${REC_SUB_CTX:-0}" = 0 ]; then
  ok t14_parity_gap "search_substrate_ctx=$SUB_CTX recall_substrate_ctx=$REC_SUB_CTX"
else
  bad t14_parity_gap "search_substrate_ctx=$SUB_CTX recall_substrate_ctx=$REC_SUB_CTX"
fi

rm -rf "$TMP"
echo "RESULT pass=$PASS fail=$FAIL"
[ "$FAIL" = 0 ]
