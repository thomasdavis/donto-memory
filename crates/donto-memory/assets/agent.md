# donto-memory — guide for AI agents

You are an AI agent. This document is your manual for using
**donto-memory** as a persistent memory layer. Read it once and
keep it referenced; everything you need is below.

**Contract version:** `0.1.0-m10`
**Base URL:** `https://memories.apexpots.com`
**License:** Apache-2.0 OR MIT.

---

## 0. TL;DR for agents

You have two endpoints to know:

  | Verb | Path | Purpose |
  |---|---|---|
  | **POST** | **`/memorize`** | Send what you want remembered. We store the raw text and use an LLM to extract every ontological statement it implies. |
  | **POST** | **`/recall`** | Send a query and a holder. We return everything we've stored that matches — facts, anchors, contradictions, the chain back to the source. |

That's it. Everything else is optional polish (manual structured
ingest, identity lenses, bitemporal time-travel, policy gating,
reconsolidation queue inspection).

**If you are integrating an existing conversational backend** (a
chat bot, a Discord/Slack agent, a web assistant) into
donto-memory, jump to [`/integration-patterns.md`](/integration-patterns.md)
after reading §0–§4 here. It's a concrete, code-first ship guide
covering context-shaping, recall on the prompt path, mode policy,
preference shortcuts, source registration, and a recommended
ship order.

### The 30-second quick-start

```bash
# Save a memory.
curl -X POST https://memories.apexpots.com/memorize \
  -H 'Content-Type: application/json' \
  -d '{
    "holder": "agent:my-bot",
    "session_id": "conversation-2026-05-28",
    "text": "The user told me they prefer vegetarian restaurants and live in Brooklyn."
  }'

# Recall it.
curl -X POST https://memories.apexpots.com/recall \
  -H 'Content-Type: application/json' \
  -d '{
    "holder": "agent:my-bot",
    "action": "read_content",
    "query": "vegetarian",
    "limit": 20
  }'
```

The first call returned ~50-200 facts the LLM extracted. The second
call surfaced them all with policy + ranking applied.

---

## 1. What this is, and what it is NOT

### What this is

A persistent memory layer for long-lived AI agents. You write
memories as plain text. We store the text *and* break it down into
typed ontological statements via an LLM, save everything to the
underlying [donto](https://github.com/thomasdavis/donto) evidence
substrate, and serve it back to you on recall — with policy
gating, identity-lens resolution, and bitemporal time-travel for
free.

### What this is NOT

  - **Not a vector database.** We use predicate alignment + identity
    lenses for semantic similarity. Vector embeddings are an optional
    follow-on (M11.x).
  - **Not a chat history.** You *can* dump every turn in via
    `/memorize`, but the value-add is the structured recall, the
    contradiction handling, and the policy gate.
  - **Not a model.** We store facts about what your user has said,
    what you've inferred, what your user prefers. The reasoning still
    happens in you.
  - **Not a sandbox.** Memories you save are *real persistence*,
    governed by attestations + policy capsules in the substrate. If
    you save something then change your mind, *write a correction*
    (a new claim with a `supersedes` argument edge) rather than
    delete — donto preserves history forever.

### Three commitments we make to you

1. **No silent rewrite.** When you re-memorize a contradicting
   fact, we don't overwrite the old one. Both live forever. Recall
   surfaces the latest by default and flags the contradiction. You
   choose what to do.
2. **Read events are not belief events.** Calling `/recall` does
   *not* affect the truth value of any claim. It bumps a private
   recall counter so reconsolidation can prioritise. The substrate's
   `tx_time` (when we believed something) is untouched.
3. **Policy-aware by default.** Every recall passes through the
   substrate's Trust Kernel. If a memory's source policy denies
   `read_content`, we return the row's metadata but redact its
   content. You always get a clear signal of what's permitted.

---

## 2. The save-memory contract

When you call `POST /memorize`, this happens:

1. **The raw text is stored** as an *episodic chunk* under your
   holder's namespace at
   `ctx:memory/episodic/session/<session_id>`. This is the
   canonical bytes-on-disk version of what you sent.
2. **An LLM (default `z-ai/glm-5`) processes the chunk** through
   one of two modes:
   - `single` (one prompt, ~30-100 facts, ~30-100 s on z-ai/glm-5).
   - `exhaustive` (five parallel aperture prompts —
     surface, linguistic, presupposition, inferential,
     conceivable — ~80-250 facts, ~60-180 s, ~5× tokens). **This is
     the default**.
3. **Every extracted fact** is asserted into the substrate as a
   typed claim `(subject, predicate, object)` with a
   `source_record_iri` link back to the original episodic chunk.
4. **You receive** the episodic record IDs + every semantic record
   ID + per-aperture yields + token usage. Total round-trip on a
   typical paragraph is 60-180 seconds.

### What gets extracted

The five apertures cover, in increasing speculation:

  - **Surface** — explicitly stated. `"The user lives in Brooklyn"`
    yields `(user, ex:residesIn, brooklyn)`.
  - **Linguistic** — every clause decomposed. `"They prefer vegetarian
    restaurants"` yields type assertions (`user rdf:type ex:Person`,
    `restaurant rdf:type ex:Restaurant`), the relation itself
    (`user ex:prefers restaurant`), and properties
    (`restaurant ex:cuisine vegetarian`).
  - **Presupposition** — what the chunk takes for granted.
    `"told me"` presupposes (user exists, agent exists, communication
    is possible). Marked `hypothesis_only=true`.
  - **Inferential** — what follows from the stated facts via common
    knowledge. `"lives in Brooklyn"` yields `(user, ex:locatedIn,
    new-york-city)`, `(user, ex:locatedIn, usa)`.
  - **Conceivable** — what could plausibly hold. `(user, ex:hasFingers,
    10)`, `(restaurant, ex:hasMenu, true)`. Marked `hypothesis_only`.

For a 60-word memory, exhaustive mode typically yields **80-300
facts**. For a 5-word memory it might yield 15-40.

### When to choose `single` vs `exhaustive`

Pass `"mode": "single"` in the request body if:
  - You're saving high-volume short chunks (chat turns, log lines).
  - You need <10 s latency.
  - You're memorising something you don't expect to query semantically.

Default (`exhaustive`) is correct for:
  - User preferences, profile facts, dossiers.
  - Important conversations.
  - Anything you'll later want to retrieve under a different surface
    form than how it was originally said.

---

## 3. The recall contract

When you call `POST /recall`, this happens:

1. **Module dispatch.** Every enabled module (`episodic`,
   `semantic-claim`, `preference`) runs its own retrieval against
   the substrate. By default, all three modules contribute.
2. **Policy gate.** Every candidate row passes through the
   substrate's Trust Kernel. If `holder` is not attested for
   `action` on the row's source policy, `action_allowed=false`.
3. **Identity-lens resolution.** If `lens_name` is set, the substrate
   returns the cluster representative for each subject/object — so
   queries about "Annie Davis" also surface "Mrs Watson" if a
   `likely_identity_v1` cluster joins them.
4. **Bitemporal time-travel.** If `as_of_tx` is set, you get the
   rows the substrate *believed* at that timestamp. The "what did
   we know last Tuesday" query.
5. **Fusion.** Module candidates are merged via Reciprocal Rank
   Fusion (k=60). A row surfaced by multiple modules ranks higher.
6. **Side effects.** Each recalled row writes an access event +
   bumps recall state + enqueues a reconsolidation task. None of
   these touch the substrate's belief state.

The returned `MemoryEvidenceBundle` includes:
  - `rows`: ranked list of statements with their full provenance.
  - `effective_actions`: per-row map of action → allowed boolean.
  - `action_allowed`: shortcut for the requested action.
  - `policy_report`: rolled-up policy summary.
  - `modules_used`: which modules contributed.

---

## 4. Endpoint reference

### `POST /memorize`

Save a memory. Episodic + (optional) LLM extraction.

**Request:**
```json
{
  "holder":      "agent:my-bot",         // required: agent IRI
  "text":        "...",                   // required: the memory
  "session_id":  "conversation-id",       // optional: scope
  "modality":    "model_output",          // optional: see below
  "extract":     true,                    // optional: false = no LLM
  "mode":        "exhaustive",            // optional: single | exhaustive
  "images":      [                        // optional: see §4.0 below
    "https://example.com/photo.jpg",
    "data:image/png;base64,iVBORw0K…"
  ]
}
```

#### Multimodal memories (images)

When `images` is non-empty, donto-memory switches the LLM call to
OpenAI multimodal message format and (if `DONTO_MEMORY_LLM_VISION_MODEL`
is set on the runtime) uses the configured vision model — currently
`openai/gpt-4o-mini` in production. Each entry is one of:

  - An **http(s) URL** the LLM provider can fetch directly. Hotlink
    protection on the host (e.g. Wikimedia returns 400 to LLM
    providers) is the most common cause of failure — host images on
    a CDN you control, or pre-fetch them yourself and inline as a
    data URL.
  - A **`data:image/...;base64,…` data URL** with the bytes inline.
    Use this for screenshots, uploads, or anything not on a public
    HTTP host. Note that very small or malformed PNGs may be
    rejected with `image_parse_error` by the provider; sanity-check
    by viewing the data URL in a browser first.

**OCR is automatic.** Before the main extraction runs, donto-memory
makes one extra vision-LLM call asking the model to transcribe every
word visible in the image(s). The transcribed text is appended to
your memory's `text` field as:

```
<your original text>

[OCR text from image #1]
<transcribed text from the first image>

[OCR text from image #2]
<transcribed text from the second image>
```

The augmented chunk becomes the episodic record and seeds the
structured-fact extraction, so visible labels, screenshots, signs,
code, watermarks, captions, etc. all become searchable via
`POST /recall query=<keyword>` later. If an image has no text, no
block is appended for that index.

Disable via `DONTO_MEMORY_OCR_ENABLED=false` on the runtime if you
want raw image-content extraction only. An OCR failure is logged
as a warning and the regular extraction still runs.

A typical landscape photo at 512×512 yields ~25 facts in ~13 s on
`openai/gpt-4o-mini`. The extracted statements look the same as
text-extracted ones — typed triples about objects in the scene,
their relations, their properties:

```json
{ "subject": "ctx:landscape/1", "predicate": "ex:hasElement",
  "object_iri": "ctx:road/1", "confidence": 0.9 }
{ "subject": "ctx:road/1", "predicate": "ex:hasType",
  "object_lit": {"v": "winding road", "dt": "string"} }
```

A few practical notes:

  - You can mix text + images in the same call: the `text` field
    becomes the focal narration; the model sees the images alongside
    it and extracts facts about both.
  - **`mode: "exhaustive"` with images is expensive** (5 parallel
    vision calls). For most multimodal use cases `mode: "single"`
    is right.
  - The episodic chunk stored under
    `ctx:memory/episodic/session/<id>` is the **text** you sent —
    not the image bytes. If you want the image content-addressed +
    tombstoneable, register it as a `donto_blob` first (see §13)
    and reference its IRI from `source_record_iri`.

**Response:**
```json
{
  "holder": "agent:my-bot",
  "session_id": "conversation-id",
  "episodic_record_id": "uuid",
  "episodic_record_iri": "ctx:memory/episodic/uuid",
  "extracted": true,
  "extract_mode": "exhaustive",
  "facts_extracted": 244,
  "facts_ingested": 244,
  "dedup_collisions": 6,
  "semantic_record_ids": ["uuid", ...],
  "model": "z-ai/glm-5",
  "usage": {
    "prompt_tokens": 1820,
    "completion_tokens": 18540,
    "total_tokens": 20360
  },
  "aperture_yields": [
    { "aperture": "surface",        "raw_facts": 22, "elapsed_ms": 8430 },
    { "aperture": "linguistic",     "raw_facts": 58, "elapsed_ms": 12100 },
    { "aperture": "presupposition", "raw_facts": 31, "elapsed_ms": 9210 },
    { "aperture": "inferential",    "raw_facts": 39, "elapsed_ms": 11380 },
    { "aperture": "conceivable",    "raw_facts": 100,"elapsed_ms": 18920 }
  ],
  "elapsed_ms": 60410,
  "warnings": []
}
```

#### Modality values

The `modality` field tags how the chunk came to exist. Pick one:

  - `model_output` (default) — an LLM produced this. Most chat-agent
    memories.
  - `descriptive` — observational. A sensor reading; a database row.
  - `oral_history` — a person told you this.
  - `community_protocol` — a community's stated norm.
  - `inferred` — you derived this from other claims.
  - `reconstructed` — you partially reconstructed this from
    incomplete evidence.
  - `elicited` — you specifically asked a question to get this.
  - `experimental_result` — output of an experiment.
  - `clinical_observation` — from a medical/clinical setting.

The substrate uses modality for downstream filtering (queries can
restrict by modality).

#### Errors

  | Code | Cause |
  |---|---|
  | 400 | `text` is empty, or `mode` is not `single`/`exhaustive`. |
  | 500 | LLM call failed (in `warnings` array, episodic still saved). |

If the LLM is unconfigured server-side, `facts_extracted = 0` and
`warnings` contains `"LLM not configured"`. The episodic chunk is
always saved regardless.

---

### `POST /memorize/batch`

Process multiple chunks in sequence. Same per-item contract as
`/memorize`; failures don't abort the rest.

**Request:**
```json
{
  "items": [
    { "holder": "agent:my-bot", "text": "chunk 1" },
    { "holder": "agent:my-bot", "text": "chunk 2", "mode": "single" }
  ]
}
```

**Response:**
```json
{
  "results": [
    { "...full /memorize response..." },
    { "...full /memorize response..." }
  ]
}
```

Items can override `mode` independently.

---

### `POST /recall`

Get a Memory Evidence Bundle.

**Request:**
```json
{
  "holder":         "agent:my-bot",       // required
  "action":         "read_content",        // optional, default read_content
  "query":          "vegetarian",          // optional: free-text filter
  "session_id":     "conversation-id",     // optional: scope
  "subject":        "ex:user-123",         // optional: narrow by subject
  "predicate":      "ex:residesIn",        // optional: narrow by predicate
  "object_iri":     "ex:brooklyn",         // optional: narrow by object
  "module_iris":    ["mem:module/episodic"], // optional: restrict modules
  "lens_name":      "strict_identity_v1",  // optional: identity lens
  "as_of_tx":       "2026-05-01T00:00:00Z",// optional: bitemporal time-travel
  "polarity":       "asserted",            // optional: default asserted
  "min_maturity":   0,                     // optional: 0..4
  "limit":          50,                    // optional: default 50, max 500
  "permitted_only": true                   // optional: default true
}
```

**Response (the Memory Evidence Bundle):**
```json
{
  "holder": "agent:my-bot",
  "action": "read_content",
  "lens":   null,
  "as_of":  null,
  "rows": [
    {
      "statement_id":     "uuid",
      "subject":          "ex:user-123",
      "predicate":        "ex:residesIn",
      "object_iri":       "ex:brooklyn",
      "object_lit":       null,
      "context":          "ctx:memory/claims/session/conversation-id",
      "polarity":         "asserted",
      "maturity":         1,
      "tx_lo":            "2026-05-28T08:30:00Z",
      "tx_hi":            null,
      "resolved_subject": "ex:user-123",
      "resolved_object":  "ex:brooklyn",
      "effective_actions": {
        "read_metadata":  true,
        "read_content":   true,
        "quote":          false,
        ...
      },
      "action_allowed":   true,
      "record_iri":       "ctx:memory/claim/uuid",
      "module_iri":       "mem:module/semantic-claim",
      "score":            0.0327868,
      "rank":             1
    }
  ],
  "row_count":     1,
  "modules_used":  ["mem:module/episodic", "mem:module/semantic-claim", "mem:module/preference"],
  "policy_report": { "permitted_only": true, "default_action": "read_content" }
}
```

#### Common recall patterns

**Get all preferences for a user:**
```json
{ "holder": "agent:my-bot", "module_iris": ["mem:module/preference"], "limit": 100 }
```

**Find contradictory facts (mixed polarities):**
```json
{ "holder": "agent:my-bot", "polarity": "any", "subject": "ex:annie-davis" }
```
Then inspect rows for the same `(subject, predicate)` with different
`polarity` or `object_iri`/`object_lit`.

**Time-travel — what did we believe last week?**
```json
{ "holder": "agent:my-bot", "as_of_tx": "2026-05-21T00:00:00Z" }
```

**Semantic-similar across aliases:**
```json
{ "holder": "agent:my-bot", "lens_name": "likely_identity_v1",
  "subject": "ex:annie-davis" }
```
Returns rows about the canonical Annie + her known aliases (Mrs
Watson, Mary Watson, etc.) under the `likely` identity lens.

---

### `POST /ingest/{module}`

Bypass the LLM and write directly into a specific module. Use when
you already have structured facts.

**Modules:** `episodic`, `semantic-claim`, `preference` (or full
IRIs `mem:module/...`).

**Episodic — just raw text:**
```json
{ "holder": "agent:my-bot", "text": "..." }
```

**Semantic-claim — typed triple:**
```json
{
  "holder":    "agent:my-bot",
  "subject":   "ex:user-123",
  "predicate": "ex:residesIn",
  "object_iri": "ex:brooklyn"
}
```
or for a literal object:
```json
{
  "holder":    "agent:my-bot",
  "subject":   "ex:user-123",
  "predicate": "ex:age",
  "object_lit": { "v": 34, "dt": "xsd:integer" }
}
```

**Preference — append-only key/value:**
```json
{
  "holder": "agent:my-bot",
  "key":    "tone",
  "value":  "casual"
}
```
A subsequent preference with the same key + a different value
creates a *new* claim plus a `supersedes` argument edge to the old.
Both live forever; recall returns the most recent.

---

### `GET /modules`

List the modules this runtime has registered. Returns the runtime
spec for each plus the DB row (so you can see if a module is
enabled but not loaded, or vice versa).

---

### `POST /reconsolidate/enqueue`

Manually request reconsolidation of a record. The sleep-path worker
picks it up in the next poll cycle (default 5 s).

```json
{
  "record_id": "uuid",
  "reason": "explicit",
  "priority": 0.5
}
```

Reasons: `recall` (set automatically on every recall), `contradiction`,
`policy_change`, `scheduled_review`, `explicit` (manual).

### `GET /reconsolidate/queue`

Inspect the head of the queue (up to 100 items).

---

### `GET /substrate`

Echo the substrate's contract version + health. Useful for
verifying donto-memory is bound to the substrate you expect.

### `GET /health`, `GET /version`

Liveness + version metadata.

---

## 5. Identity lenses

The donto substrate stores *every* entity reference verbatim. When
two references actually refer to the same real-world entity, we
record that as a weighted identity edge — never collapse the rows.
At query time, the **identity lens** parameter controls how strict
the equivalence judgement is.

Default seeded lenses:
  - `strict_identity_v1` — only edges with confidence ≥ 0.98.
  - `likely_identity_v1` — ≥ 0.85.
  - `exploratory_identity_v1` — ≥ 0.60.

If you ask for `"lens_name": "strict_identity_v1"`, a query about
`ex:annie-davis` will return *only* rows whose subject is provably
the same Annie (≥ 0.98 confidence). With `exploratory_identity_v1`
you'll see a wider net — including merge candidates the substrate
isn't sure about.

For most agent workloads, `null` (no lens) is the right default —
treat every IRI as itself, no expansion. Use `likely_identity_v1`
when surfacing memories about a person whose name varies across
sources.

---

## 6. Bitemporal time-travel

Every claim has two times:

  - **`valid_time`** — when the fact was true in the world.
  - **`tx_time`** — when we believed it.

Recall with `"as_of_tx": "2026-05-01T00:00:00Z"` returns the rows
that were *currently believed* on that date. If a fact was retracted
on 2026-05-15, an `as_of_tx=2026-05-10` query still sees it. This is
the "what did we know on date X?" pattern.

For valid-time queries (claims whose worldly validity intersects a
target date), pass `"as_of_valid"` instead. Less common in
agentic-memory workloads but available.

---

## 7. Policy actions

Every recall asks for a specific action. The substrate gates each
row based on the source's policy capsule + the holder's
attestation. The 16 actions:

  - `read_metadata` — see that the row exists. Default-permitted.
  - `read_content` — read the actual values. The common agent recall
    case.
  - `quote` — include verbatim in a user-visible answer.
  - `view_anchor_location` — see *where in the source* the claim was
    extracted.
  - `derive_claims` — extract new derived statements.
  - `derive_embeddings` — generate embeddings.
  - `translate` — translate the content.
  - `summarize` — produce a summary.
  - `export_claims` — include in a release.
  - `export_sources` — include the source.
  - `export_anchors` — include the anchor locations.
  - `train_model` — use in model training.
  - `publish_release` — include in a citable release.
  - `share_with_third_party` — pass to another agent/system.
  - `federated_query` — answer a federated query against another
    donto instance.
  - `request_deletion` — initiate tombstoning.

Most agent workflows want `read_content` (or `quote` if you'll be
showing the text directly to a user).

When a row is denied, you still get the row in the bundle — the
substrate is *transparent about denial*, not silent — but
`action_allowed=false`. Use `permitted_only=true` to filter to only
allowed rows.

---

## 8. Cost expectations

LLM token cost dominates `/memorize` cost.

  - `mode: "single"` — 1 LLM call. On `z-ai/glm-5`:
    ~300 prompt + ~3-8 K completion = **~$0.005-$0.015** per memorize
    on OpenRouter.
  - `mode: "exhaustive"` — 5 parallel LLM calls. ~1.8 K prompt + ~15-25 K
    completion total = **~$0.04-$0.07** per memorize.

Recall has no LLM cost. Latency depends almost entirely on the
substrate-side `/recall` pipeline:

  - **Small / per-user holders** (a few dozen records): 30–80 ms.
    This is the omega-bot per-user `session_id: discord:user:<id>`
    pattern — recall feels instant.
  - **Big session contexts** (hundreds-to-thousands of records per
    session, e.g. a channel-scoped session for an active Discord
    server): **3–5 s**. The substrate's policy gate + identity-lens
    pipeline scans every candidate row in the context, so latency
    scales with session size.

The donto-memory hot-path composer adds ~30–100 ms on top of
whatever the substrate returns (module fan-out, RRF fusion,
per-row access bookkeeping in parallel). So `/recall` latency is
dominated by which substrate-side path your session triggers.

**Practical implication.** For the response hot path of a
conversational agent, prefer **per-user `session_id`** (Tier 2.6 in
the integration spec) over per-channel. That keeps each user's
session small and recall fast. If you must use a per-channel
session, wrap the recall in a `Promise.race` against a 500 ms
timer and fall through to the base prompt — losing context is
better than losing the turn.

If you're processing thousands of memorize calls/day, `single` mode
is the right default. If you're processing dozens but they're
important (user profile facts, key conversations), use `exhaustive`.

---

## 9. Failure modes + retries

**The episodic chunk is always saved**, even if the LLM call fails.
You'll get a `warnings` array in the response describing what went
wrong. If the LLM is unconfigured, you get a warning + 0 facts.

  | Symptom | Fix |
  |---|---|
  | HTTP 400 | Validate request: `text` is required + non-empty. |
  | HTTP 500 + warnings | LLM call failed. The chunk was saved. Retry later or call `/reconsolidate/enqueue` to re-extract. |
  | Timeout | `/memorize?mode=exhaustive` takes 60-180 s on z-ai/glm-5. Cloudflare's free-tier proxy times out at 100 s — call the host on its private IP for long jobs, or use `mode=single`. |
  | Recall returns 0 rows for a query you just memorized | The substrate's `/recall` is real-time; if no rows appear, check `holder`, `session_id`, and your free-text filter. |
  | Recall returns `action_allowed=false` everywhere | The default policy is fail-closed for most actions. Use `read_metadata` (always allowed) to see what's there, or request an attestation for the action you need. |

Retries are safe: `/memorize` is idempotent at the substrate level
(the substrate dedups by content hash), so repeated calls of the
same chunk produce only one episodic statement. The semantic
extraction will produce some new variant facts on each call but
won't *contradict* — donto preserves contradictions if they happen.

---

## 10. Substrate concepts you should know

donto-memory is built on **donto**, an evidence-grade quad store.
Concepts that leak into the API:

  - **Statement** — `(subject, predicate, object)` quad, filed under
    a `context`. The atomic unit of belief.
  - **Context** — an IRI grouping statements. Yours live under
    `ctx:memory/episodic/session/<session_id>` and
    `ctx:memory/claims/session/<session_id>`.
  - **Polarity** — `asserted | negated | absent | unknown`. Donto
    keeps both `X bornIn Y` and `X NOT bornIn Y` if two sources
    disagree.
  - **Maturity** — 0 (raw) to 4 (corroborated). Memorize-extracted
    facts land at 1 (candidate). Reviewer-promoted facts can reach
    higher.
  - **tx_time / valid_time** — system time vs world time. See
    bitemporal section.
  - **Modality** — see modality values above.
  - **Policy capsule** — a named bundle of allowed actions. Every
    `donto_document` has one; defaults fail-closed.
  - **Attestation** — credential granting a holder specific actions
    under a policy.
  - **Identity hypothesis** — named clustering of `same_referent`
    edges. Strict / likely / exploratory.

You don't need to manipulate these directly to use donto-memory.
But seeing them in the recall response makes more sense once you
know the names.

---

## 11. Cookbook: common agent patterns

### Pattern A — Long-term user profile

```python
# Whenever the user shares a profile fact, memorize it.
memorize(holder="agent:my-bot", session_id=f"user/{user_id}",
         text=user_message, mode="exhaustive")

# Before generating a response, recall.
bundle = recall(holder="agent:my-bot", session_id=f"user/{user_id}",
                action="read_content", limit=50)
context_for_llm = format_bundle_for_prompt(bundle)
```

### Pattern B — Preference tracking

```python
# When a user says "I prefer X", use the preference module directly.
ingest("preference", holder="agent:my-bot",
       key="preferred_tone", value="casual")

# Later, retrieve.
prefs = recall(holder="agent:my-bot",
               module_iris=["mem:module/preference"], limit=100)
```

A preference change is a new statement + a `supersedes` argument
edge. Both live forever. The new value is what shows up first in
recall.

### Pattern C — Conversation memory

```python
# After each turn, save the full exchange.
memorize(holder="agent:my-bot",
         session_id=f"conv/{conv_id}",
         text=f"User: {user_msg}\nMe: {my_response}",
         modality="model_output", mode="single")

# Mid-conversation recall to find earlier context.
bundle = recall(holder="agent:my-bot",
                session_id=f"conv/{conv_id}",
                query=topic_keyword, limit=10)
```

### Pattern D — Multi-agent shared memory

Two agents can share a holder if their access patterns are
compatible. Both can recall what either saved. If you want
isolation, use distinct holder IRIs (e.g. `agent:assistant`,
`agent:summariser`).

For cross-agent reads with policy gating, get an attestation: ask
the operator to issue a `donto_attestation` to your agent IRI for
the action you need.

### Pattern E — "What did I know on date X?"

```python
bundle = recall(holder="agent:my-bot",
                as_of_tx="2026-05-01T00:00:00Z",
                subject="ex:annie-davis")
```

Use case: an agent wants to roll back its understanding to before a
correction was applied, or wants to know "what would I have said
last week?"

---

## 12. Things you should NOT do

  - **Don't try to delete memories.** Use `/reconsolidate/enqueue`
    with reason=`policy_change` if you want a memory's policy
    re-evaluated. Tombstoning (true deletion) requires an
    attestation and goes through `donto_blob_tombstone` on the
    substrate side — out of band of donto-memory's API.
  - **Don't include API keys or secrets in `text`.** Memories are
    persisted; you can't unsay them. The substrate has a
    `request_deletion` path but it's deliberately heavyweight.
  - **Don't memorize the same chunk repeatedly to "boost" recall.**
    Donto deduplicates at the content-hash level; you'll just
    increment access events. Use `priority` on
    `/reconsolidate/enqueue` if you want a specific record looked
    at sooner.
  - **Don't rely on `mode: "exhaustive"` for sub-second latency.**
    Five LLM calls in parallel still take 60-180 s. Use `single` for
    real-time paths.
  - **Don't bypass `/memorize` to write structured facts unless you
    really mean it.** If you call `/ingest/semantic-claim` directly
    without an episodic anchor, the claim has no provenance — donto
    will accept it but downstream review tools won't be able to
    follow the chain back.

---

## 13. Storing source documents alongside memories (`donto blob`)

The donto substrate has a content-addressed blob store
(`donto_blob`) and a document-with-revisions layer
(`donto_document` / `donto_document_revision`) sitting underneath
the statement table. donto-memory's `/memorize` does **not** use
either by default — it stores your text as a `xsd:string` literal
inside an `mem:episodic/chunk` statement. That works for short
messages, but it skips four things you usually want for **the raw
message itself**:

  1. **Content-addressing** — identical messages dedupe to one
     SHA-256-keyed blob. Today the substrate holds **50,000+ blobs
     totalling ~6 GB**; your messages join that pool.
  2. **Policy capsules** — a document is governed by a named policy
     (`policy:user-conversation`, `policy:agent-internal`, etc.).
     Revoking access to all messages under one policy is a single
     attestation flip; revoking access to text-inside-statements
     means walking every `donto_statement`.
  3. **Revision history** — corrections, edits, redactions of the
     raw message land as new revisions instead of mutating the
     original.
  4. **Tombstones** — `donto blob tombstone` is the only way to
     drop a blob's bytes from disk while preserving the
     fact-of-deletion (who, when, under what attestation, why). If
     you only `/memorize`, you cannot tombstone.

### When to register a source document

For **most short utterances** (one user turn, one log line, one
preference), skip this section — `/memorize` alone is correct.

For **anything you might want to tombstone, share verbatim, or
re-version later** — long-form messages, transcripts, PDFs,
uploaded files, screenshots, agent system prompts you're storing
for review — register a document on the substrate **first**, then
call `/memorize` referencing it.

### The two-step substrate flow

donto-memory does not proxy these endpoints; you call dontosrv
directly. Substrate base: the same VM as donto-memory, port 7879
(local). Replace with your substrate's host.

**Step 1 — Register the source + policy.** Substrate base
`http://localhost:7879` (or your deployment's substrate URL):

```bash
curl -X POST $DONTOSRV/sources/register \
  -H 'Content-Type: application/json' \
  -d '{
    "iri":         "doc:my-bot/discord-message/1497274794586931220",
    "source_kind": "agent-message",
    "policy_iri":  "policy:user-conversation",
    "media_type":  "text/markdown",
    "label":       "Discord #donto turn",
    "source_url":  "https://discord.com/channels/.../1497274794586931220"
  }'
```

Returns `{document_id, iri, policy_iri}`. The `policy_iri` MUST
exist; the substrate fails closed.

**Step 2 — Attach the body as a revision.** This is where the bytes
become content-addressed:

```bash
curl -X POST $DONTOSRV/documents/revision \
  -H 'Content-Type: application/json' \
  -d '{
    "document_id":    "<uuid from step 1>",
    "body":           "ajaxdavis in #donto: a dog fell into river and hunted fish",
    "parser_version": "raw/v1"
  }'
```

Returns `{revision_id}`. The substrate computes the SHA-256, dedupes
against existing blobs, and writes a `donto_document_revision` row
linking your document to the (possibly pre-existing) blob.

**Step 3 — Memorize, anchored to the document.** Now call
donto-memory and point it at the document IRI you just created:

```bash
curl -X POST https://memories.apexpots.com/memorize \
  -H 'Content-Type: application/json' \
  -d '{
    "holder":     "agent:my-bot",
    "session_id": "discord:server-id:channel-id",
    "text":       "ajaxdavis in #donto: a dog fell into river and hunted fish",
    "source_record_iri": "doc:my-bot/discord-message/1497274794586931220"
  }'
```

The episodic chunk + every extracted fact now has a stable pointer
back to your document IRI. Recall will surface that IRI in
`record_iri` / `subject` columns; the donto CLI's `donto blob
fetch` and `donto blob tombstone` operate on the blob behind it.

### Bulk file ingest via CLI

For uploading existing files (PDFs, GEDCOMs, scanned docs,
training data, archives), the CLI is more ergonomic than HTTP:

```bash
# One file at a time.
donto blob upload /path/to/file.pdf

# Or walk a directory recursively. Idempotent per file.
donto blob sync /path/to/research-corpus/

# Inspect what is registered.
donto blob list  --limit 20
donto blob stats           # total bytes, count, mime breakdown

# Pull a blob back out by its sha256 hex (or its document IRI).
donto blob fetch <sha256-hex> --out /tmp/recovered.pdf
```

After `donto blob upload`, the blob exists in
`donto_blob` but has no `donto_document` wrapping it. Use
`/sources/register` + `/documents/revision` (Steps 1+2 above) to
mint a document over the blob, then `/memorize` to land the
linkage in donto-memory.

### Recap: when to use each layer

| Layer | Best for | Tombstoneable? | Dedupes bytes? |
|---|---|---|---|
| `/memorize` only | Short messages, preferences, single utterances. | No — append-only literal. | No — every `/memorize` writes its own statement. |
| `/memorize` + `/sources/register` + `/documents/revision` | Any chunk you might tombstone, share, or re-version later. | **Yes** — via `donto blob tombstone`. | **Yes** — `donto_blob` is sha256-keyed. |
| `donto blob upload` (CLI) | Bulk files, batch ingest, anything off the request path. | Yes. | Yes. |

### Policy IRIs to know

The substrate ships with sensible default policy capsules. Use
whichever fits — never invent an unattested IRI:

  - `policy:user-conversation` — content the user authored;
    `read_content` permitted to the agent, `share_with_third_party`
    requires attestation.
  - `policy:agent-internal` — the agent's own scratch / state;
    `read_content` permitted to the agent, everything else closed.
  - `policy:public-corpus` — public reference material;
    everything except `train_model` and `request_deletion` permitted.

If you need a custom policy capsule, ask the substrate operator to
mint it via `donto policy register`. Do **not** call
`/sources/register` with a non-existent `policy_iri` — the
substrate will accept the call, but every `read_content` recall on
that document will fail closed.

---

## 14. Operator surfaces & the ops token

The `/jobs/*` and `/explore/*` paths expose every memorized text +
recall query body. They are designed as **observability tools for
the operator**, not as public agent surfaces. On any deployment
that's reachable from the public internet, set
`DONTO_MEMORY_OPS_TOKEN=<long-random-string>` on the runtime — this
gates those 10 routes behind a bearer token:

```bash
# anonymous → 401
curl https://memories.apexpots.com/jobs                # → 401

# with the token via Authorization header
curl -H 'Authorization: Bearer <token>' \
  https://memories.apexpots.com/jobs                   # → 200

# or via query string (handy in browser bookmarks)
curl 'https://memories.apexpots.com/jobs?token=<token>'  # → 200
```

When the env var is **unset**, the routes are open (preserves the
local-dev workflow). When set, the comparison is constant-time so
the token can't be probed for length or prefix.

The **agent contract** (`/`, `/agent.md`, `/llms.txt`,
`/openapi.json`, `/docs`, `/memorize`, `/recall`, `/ingest/*`,
`/modules`, `/version`, `/health`) is **never gated** — agents and
documentation should always be reachable.

---

## 15. Resources

  - **Concrete integration patterns** (recall on the prompt path,
    conversation context shaping, mode policy, preference shortcuts,
    source registration, ship order): [`/integration-patterns.md`](/integration-patterns.md).
    Read this if you are wiring an existing conversational backend
    (Discord/Slack/web bot) into donto-memory.
  - **OpenAPI 3.1 spec:** [`/openapi.json`](/openapi.json)
  - **Swagger UI:** [`/docs`](/docs)
  - **This guide as plain text:** [`/llms.txt`](/llms.txt)
  - **Repo:** [github.com/thomasdavis/donto-memory](https://github.com/thomasdavis/donto-memory)
  - **Substrate repo:** [github.com/thomasdavis/donto](https://github.com/thomasdavis/donto)
  - **Substrate PRD (M10 hardening):** [substrate PRD](https://genes.apexpots.com/research/donto-substrate-prd-2026-05-28.html)
  - **Substrate paper:** [donto paper](https://genes.apexpots.com/research/donto-paper-2026-05-28.html)

---

*donto-memory v0.1.0 · Apache-2.0 OR MIT · Contract `0.1.0-m10`*
