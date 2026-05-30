# donto-memory — integration patterns for AI-agent backends

A working spec for the dev (human or AI agent) wiring an existing
conversational agent into donto-memory. Patterns are ordered by
impact: top of the list ships the biggest UX or quality gain per
hour of work. Code is TypeScript because most current consumers are
Node, but every snippet maps cleanly to Python or Rust — only the
HTTP shapes matter.

**Base URL:** `https://memories.apexpots.com`
**Contract version:** `0.1.0-m10`
**Agent guide (read first):** [`/agent.md`](/agent.md)
**OpenAPI:** [`/openapi.json`](/openapi.json)

---

## Tier 1 — Highest impact, do these first

### 1.1 — Structured conversation context in the `text` field

**Why:** the LLM has no surrounding context, so references like
*"yes, exactly"*, *"that one"*, or *"what they said"* become inert
fact-noise. The fix is shaping the input so the LLM extracts facts
from the **current message** while seeing context, not from the
context itself.

**Pattern.** Build the `text` field with last *N* messages
(oldest-first), a soft hint asking the model not to extract from
them, then a delimiter and the focal message:

```ts
function buildMemorizeText(
  prior: Array<{ author: string; ago: string; text: string }>,
  current: { author: string; text: string },
  channel: string,
): string {
  const ctxBlock = prior.length === 0
    ? ''
    : `Context from #${channel} (do not extract facts from these — they are for reference only):\n` +
      prior.map(m => `- ${m.author} (${m.ago}): ${m.text}`).join('\n') +
      '\n\n';
  return (
    ctxBlock +
    `Current message to analyze:\n` +
    `${current.author} (just now): ${current.text}`
  );
}
```

Use last **N=3** for `mode: 'single'`, **N=5** for
`mode: 'exhaustive'`. The "do not extract" line is a soft hint —
the LLM respects it ~80% of the time on single-mode glm-5, higher
on bigger models. Worth the tokens.

**Trade-off:** ~150–500 extra prompt tokens per call. At
$0.00005/1K input tokens (glm-5 pricing tier), negligible.

---

### 1.2 — Recall integration (the actual payoff)

**Why:** writing memories without reading them is dead weight.
Every response the agent generates should be conditioned on what
the agent already knows about the user, the topic, and the
conversation.

**Pattern.** Hit `/recall` immediately before building the system
prompt:

```ts
import { recall } from './donto-memory-client';

async function buildSystemPrompt(userId: string, username: string, userMessage: string) {
  const bundle = await recall({
    holder:        'agent:omega-bot',
    action:        'read_content',
    query:         userMessage,                 // free-text filter
    session_id:    `discord:user:${userId}`,    // narrow to this user
    limit:         20,
    permitted_only: true,                       // skip denied rows entirely
  });

  // Filter to the highest-quality rows: prefer asserted polarity,
  // higher RRF score, and skip episodic chunks (those are bytes,
  // not facts).
  const facts = bundle.rows
    .filter(r => r.module_iri !== 'mem:module/episodic')
    .filter(r => r.polarity === 'asserted')
    .slice(0, 15);

  if (facts.length === 0) {
    return BASE_SYSTEM_PROMPT;
  }

  const memoryBlock = facts
    .map(f => {
      const obj = f.object_iri ?? f.object_lit?.v ?? '?';
      return `  - ${f.subject} ${f.predicate} ${obj}`;
    })
    .join('\n');

  return `${BASE_SYSTEM_PROMPT}

What you know about ${username} (from prior interactions):
${memoryBlock}

Treat these as background context. Do not recite them verbatim
unless the user asks "what do you know about me".`;
}
```

**Latency budget.** Recall scales with session size:

  - **Per-user `session_id`** (Tier 2.6): 30–80 ms. Use this on the
    response hot path.
  - **Per-channel `session_id`**: 3–5 s on a busy channel with
    hundreds of memories. The substrate's policy gate is the
    bottleneck; donto-memory's composer adds only ~30–100 ms.

If you must use per-channel, wrap recall in a `Promise.race`
against a 500ms timer and fall through to the base prompt on
timeout — losing context is better than losing the turn. The per-
user pattern keeps the hot path fast; per-channel is fine for
"background warming" out of band.

**Action choice.** Use `read_content` for actually conditioning
the model's output. Use `read_metadata` if you only need to count
or rank memories without exposing values. Use `quote` only when
you'll show the memory text directly to the user (e.g. *"I remember
you said: X"*).

---

### 1.3 — Source registration for tombstoneable provenance

**Why:** today, memorize-only stores your text as an
`xsd:string` literal inside an `mem:episodic/chunk` statement.
That's append-only and **cannot be tombstoned** — if a user deletes
their Discord message, you can't honour the deletion downstream.

Registering each message as a `donto_document` first gives you:
- SHA-256 dedup (~50K blobs / 6 GB already in the substrate, your
  messages join that pool)
- `donto blob tombstone` per-message (Discord delete → memory
  tombstone)
- Policy capsule per message (revoke access to all messages under
  a policy in one attestation flip)
- Revision history if the user edits

**Pattern.** Two extra HTTP calls before `/memorize`. Both are
~30–50ms; both are idempotent (re-registering the same IRI is a
no-op):

```ts
const DONTOSRV = process.env.DONTOSRV_URL ?? 'http://localhost:7879';

async function registerSourceMessage(msg: DiscordMessage): Promise<string> {
  const iri = `doc:omega/discord-message/${msg.id}`;

  // Step 1: register the document + policy. Idempotent.
  const reg = await fetch(`${DONTOSRV}/sources/register`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({
      iri,
      source_kind: 'agent-message',
      policy_iri:  'policy:user-conversation',
      media_type:  'text/markdown',
      label:       `Discord ${msg.guild?.name ?? 'DM'} #${msg.channel.name ?? '?'}`,
      source_url:  msg.url,
    }),
  }).then(r => r.json());

  const documentId = reg.document_id;

  // Step 2: attach body. SHA-256 dedup happens server-side.
  await fetch(`${DONTOSRV}/documents/revision`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({
      document_id:    documentId,
      body:           msg.content,
      parser_version: 'discord/raw-v1',
    }),
  });

  return iri;  // Use as source_record_iri on /memorize.
}
```

Then on `/memorize`:

```ts
const sourceIri = await registerSourceMessage(msg);
await memorize({
  holder:            'agent:omega-bot',
  session_id:        `discord:user:${msg.author.id}`,
  text:              buildMemorizeText(prior, current, msg.channel.name),
  source_record_iri: sourceIri,
  mode:              'single',
});
```

**Tombstoning on user delete.** When Discord fires a
`messageDelete` event:

```ts
client.on('messageDelete', async (msg) => {
  const sha256 = await sha256Hex(msg.content);  // same bytes → same blob
  // Use the CLI (or wrap it as a shell-out for now; HTTP route is M11.x).
  exec(`donto blob tombstone ${sha256} ` +
       `--reason 'user-deleted-source-message' ` +
       `--authority 'discord:${msg.guildId}'`);
});
```

The fact-of-deletion is recorded; the bytes drop off disk. The
extracted facts in `donto_statement` remain — they're derived
data, governed by their own policy — but no caller can read the
original message text any more.

**Latency cost.** ~50–80ms total for the two extra calls. If you
need <50ms response, run the registration in `Promise.allSettled`
parallel with the LLM extraction; the substrate accepts
`source_record_iri` pointing at an IRI that's still being
registered, and reconciliation happens when the revision lands.

---

## Tier 2 — Cost & quality wins

### 2.4 — Mode policy by message substance

`exhaustive` costs ~$0.04–0.08 and takes 60–180s. `single` costs
~$0.015 and takes 30–100s. **Most messages don't need either of
those — skip them entirely.**

```ts
function pickMode(msg: DiscordMessage):
    'skip' | 'single' | 'exhaustive' {

  const text = msg.content.trim();

  // Skip — saves both modes' worth of cost.
  if (text.length < 4) return 'skip';
  if (text.length < 100 && /^[\p{Emoji}\s]+$/u.test(text)) return 'skip';
  if (text.startsWith('!') || text.startsWith('/')) return 'skip';      // command
  if (msg.author.bot) return 'skip';                                     // bot chatter
  if (/^(lol|haha|nice|cool|ok|yes|no|👍|👀)\s*$/i.test(text)) return 'skip';

  // Exhaustive only when there's substantive content to mine.
  const hasSubstance = /\b(i prefer|i like|i love|i hate|my favorite|i think|i believe|i remember|i feel|i live|i work|i used to)\b/i.test(text);
  if (text.length > 200 && hasSubstance) return 'exhaustive';

  // Default: cheap mode.
  return 'single';
}
```

**Expected cost drop:** ~70% on a typical Discord channel
(estimated from the omega-bot session_id distribution in the live
audit log — most messages are short reactions or commands).

---

### 2.5 — Direct `/ingest/preference` for preference statements

**Why:** the LLM round-trip is overkill when the user says
something a regex can identify. The preference module has built-in
supersession (a new value for the same key emits a `supersedes`
argument edge to the prior), is fast (~50ms total), and produces
clean rows that `/recall?module_iris=["mem:module/preference"]`
returns directly.

```ts
const PREFERENCE_PATTERNS: Array<{ regex: RegExp; key: string }> = [
  { regex: /i (?:prefer|like|love|enjoy) (.+)/i,           key: 'likes' },
  { regex: /i (?:hate|dislike|can't stand) (.+)/i,         key: 'dislikes' },
  { regex: /my favorite (\w+) is (.+)/i,                   key: 'favorite' },
  { regex: /i live in (.+)/i,                              key: 'residence' },
  { regex: /i'?m from (.+)/i,                              key: 'origin' },
  { regex: /i (?:work|live|study) (?:at|in) (.+)/i,        key: 'context' },
];

async function trySendPreference(holder: string, userId: string, text: string) {
  for (const { regex, key } of PREFERENCE_PATTERNS) {
    const m = text.match(regex);
    if (!m) continue;
    const value = m.slice(1).filter(Boolean).join(' / ').trim();
    if (value.length === 0) continue;

    await fetch(`${MEMORIES}/ingest/preference`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({
        holder,
        session_id: `discord:user:${userId}`,
        key,        // e.g. "likes" — supersedes prior "likes" automatically
        value,      // free text; literal stored as xsd:string
      }),
    });
    return true;
  }
  return false;
}

// In the handler:
const handledAsPreference = await trySendPreference(
  'agent:omega-bot', msg.author.id, msg.content,
);
if (!handledAsPreference) {
  // Fall through to normal /memorize path.
  await memorize({ ... });
}
```

Then your recall picks them out cleanly:

```ts
const prefs = await recall({
  holder:       'agent:omega-bot',
  session_id:   `discord:user:${userId}`,
  module_iris:  ['mem:module/preference'],
  limit:        50,
});
// prefs.rows: [{subject: agent:..., predicate: 'mem:pref/likes', object_lit: {v: 'Brooklyn pizza'}}]
```

---

### 2.6 — Per-user `session_id`

**Today:** `discord:${guildId}:${channelId}` — groups by channel.
Means you cannot easily ask *"what do I know about user X across
all their channels?"*

**Switch to:** `discord:user:${userId}` — groups by user, and
`/recall?session_id=...` then returns everything you know about
that person regardless of channel.

If you need both axes, encode in `text` instead of `session_id`:

```ts
const text = `In #${channelName}, ${username} (just now): ${msg.content}`;
const session_id = `discord:user:${msg.author.id}`;
```

The channel context becomes one of the extracted facts (the LLM
will type `#general` as `ex:Channel`, `username ex:saidIn
#general`, etc.) but the session boundary is what you want it to
be: the user.

**Migration.** Existing rows under `discord:guild:channel` stay
forever (donto is append-only). New rows go under `discord:user:`.
Recall queries that need to see both can pass `permitted_only:
false` and walk both session IDs. Or just accept that old data is
historical and move on.

---

## Tier 3 — Richer signal

### 3.7 — Reply-context preservation

When `message.reference` exists, fetch the parent and include it
explicitly in `text`:

```ts
async function withReplyContext(msg: DiscordMessage): Promise<string> {
  if (!msg.reference?.messageId) return msg.content;
  try {
    const parent = await msg.channel.messages.fetch(msg.reference.messageId);
    return (
      `${msg.author.username} is replying to ${parent.author.username}'s message: ` +
      `"${parent.content}"\n\n` +
      `"${msg.content}"`
    );
  } catch {
    return msg.content;
  }
}
```

This is sometimes higher-signal than the rolling-N-message context
window (Tier 1.1), because Discord replies are explicit references
the model can rely on.

### 3.8 — Image attachments (multimodal /memorize)

Discord users send screenshots and photos constantly. donto-memory
accepts them directly via the `images` field on `POST /memorize` —
each entry is an http(s) URL the LLM provider can fetch or a
`data:image/...;base64,…` data URL with the bytes inline.

When images are present, two things happen automatically before the
normal extraction:

1. **OCR pass** — one vision-LLM call transcribes every word visible
   in the image(s). The transcripts get appended to your `text`
   field as `[OCR text from image #N]\n<transcript>` blocks before
   the episodic chunk is stored. Visible labels in screenshots,
   captions on memes, code snippets, signs, watermarks — all
   become searchable later via `POST /recall query=<keyword>`.
2. **Vision extraction** — the structured-fact extractor sees the
   image alongside the augmented text and yields typed triples
   about objects in the scene plus the transcribed words.

```ts
// Discord message → /memorize with image attachments
const imageUrls = Array.from(msg.attachments.values())
  .filter(a => a.contentType?.startsWith('image/'))
  .map(a => a.url);                     // Discord CDN URLs are fetch-friendly

if (imageUrls.length > 0) {
  await fetch(`${MEMORIES}/memorize`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({
      holder:     'agent:omega-bot',
      session_id: `discord:user:${msg.author.id}`,
      text:       msg.content || '',     // safe to send empty; OCR fills it
      mode:       'single',              // exhaustive × N images is expensive
      images:     imageUrls,
    }),
  });
}
```

**Latency expectations.** Single-mode + 1 image runs in ~10-15 s on
`openai/gpt-4o-mini`. Exhaustive mode multiplies vision tokens
across 5 apertures + the OCR pass — usually only worth it for
high-value content. The OCR pass alone is ~2-5 s.

**Inline base64 (when the bytes are local).** If you can't expose a
public URL — e.g. an upload your bot hasn't forwarded yet — convert
to a data URL. Cap the payload (50 KB is fine, ~1 MB is the
practical ceiling) because the LLM API charges by image-token
count, and the donto-memory audit log stores a truncated marker
rather than the bytes themselves.

```ts
const dataUrl = `data:image/png;base64,${buf.toString('base64')}`;
// Then images: [dataUrl] as above.
```

**Tombstoning images.** OCR text + extracted facts land in
donto_statement (append-only, paraconsistent). The image bytes
themselves are NOT stored by donto-memory — only the URL or the
truncated audit marker. If you need the image bytes content-
addressed and tombstoneable later (Discord delete → memory delete),
register them as a `donto_blob` first per §1.3, then call
`/memorize` with both `images: [...]` and `source_record_iri`
pointing at the document IRI.

**Disable OCR.** Set `DONTO_MEMORY_OCR_ENABLED=false` on the runtime
if you only want vision fact extraction without the text-transcribe
pre-pass. The image still gets sent to the extractor; you just skip
the OCR round-trip.

### 3.8a — Non-image attachments and bare links

For non-image attachments (PDFs, audio, video) and naked URLs in the
message body, prepend a soft marker so the LLM can extract facts
about them even without seeing the bytes:

```ts
function annotateAttachments(msg: DiscordMessage): string {
  const tags: string[] = [];
  for (const att of msg.attachments.values()) {
    if (att.contentType?.startsWith('image/')) continue;   // handled by §3.8
    tags.push(`[attachment ${att.contentType ?? 'unknown'}: ${att.name}]`);
  }
  for (const url of (msg.content.match(/https?:\/\/\S+/g) ?? [])) {
    tags.push(`[link: ${url}]`);
  }
  return tags.length === 0
    ? msg.content
    : `${tags.join(' ')}\n${msg.content}`;
}
```

The LLM produces facts like
`(msg, ex:hasAttachment, "report.pdf")` and
`(msg, ex:references, "https://example.com/article")`. If you later
register the attachment as a `donto_blob` (via `donto blob
upload`), the fact has a real anchor; otherwise it's a string
literal pointer.

### 3.9 — `/memorize/batch` during bursts

Discord spam (command chains, copy-paste floods, raid traffic)
generates one HTTP call per message. Use the batch endpoint with a
short debounce window:

```ts
const queue: MemorizeReq[] = [];
let timer: NodeJS.Timeout | null = null;

function enqueueMemorize(req: MemorizeReq) {
  queue.push(req);
  if (timer) return;
  timer = setTimeout(async () => {
    const batch = queue.splice(0);
    timer = null;
    if (batch.length === 0) return;
    await fetch(`${MEMORIES}/memorize/batch`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ items: batch }),
    });
  }, 5_000);  // 5-second debounce; pick based on your traffic profile
}
```

**Note.** `/memorize/batch` is *serial* server-side
(`memorize_one` runs in a for-loop). You get audit-log bundling
and one HTTP round-trip, but no parallelism on the LLM calls.
Don't expect it to compress wall-time on exhaustive mode.

---

## Production hardening — set the ops token

`/jobs/*` and `/explore/*` on the donto-memory instance expose every
memorized text + recall query body — they're operator tools, not
agent surfaces. On any deployment reachable from the public
internet, the operator should set
`DONTO_MEMORY_OPS_TOKEN=<random-token>` so anonymous visitors get
401 on those routes.

That doesn't affect any agent code: `/memorize`, `/recall`,
`/ingest/*`, `/agent.md`, `/openapi.json` are never gated. The
bot's HTTP calls keep working unchanged. The only place that breaks
is anyone who was reading the operator dashboard without a token —
they need to pass `Authorization: Bearer <token>` or `?token=<token>`.

You can detect whether a deployment has the gate active with one
call:

```ts
const { ops_token_required } =
  await fetch(`${MEMORIES}/api`).then(r => r.json());
if (ops_token_required) {
  console.warn('ops surfaces locked — supply token to access /jobs and /explore');
}
```

## Tier 4 — Skip for now

- **Identity lenses** (`lens_name: 'likely_identity_v1'`) —
  unnecessary until you have multiple Discord identities resolved
  to the same person across servers.
- **Bitemporal `as_of_tx` recall** — useful for "what did I know
  last week" debugging, not the response hot-path.
- **Custom policy capsules** — `policy:user-conversation` covers
  the Discord case. Mint a custom one only when you need to
  distinguish (e.g.) DM messages from public-channel messages
  under different access rules.

---

## Suggested ship order

| Step | Tier | Effort | Impact |
|---|---|---|---|
| Structured `text` with context (§1.1) | T1 | 30 min | biggest extraction-quality bump |
| Recall integration (§1.2) | T1 | 1–2 h | biggest UX bump |
| Mode policy + skip rules (§2.4) | T2 | 30 min | cuts cost ~70% |
| Preference direct-ingest (§2.5) | T2 | 30 min | preferences instant + clean |
| Source registration (§1.3) | T1 | 1 h | unlocks tombstoning |
| Image attachments via `images: [...]` (§3.8) | T3 | 30 min | screenshots/photos become first-class memories with OCR |
| Reply + non-image attachment context (§3.7, §3.8a) | T3 | 1 h | richer fact graph |
| Batch on bursts (§3.9) | T3 | 30 min | only if rate-limited |

Total: ~5.5 hours for the full integration upgrade.

---

## Quick reference — current API shapes

The endpoints these patterns hit, with their minimal request bodies:

```ts
// POST /memorize → memorize.MemorizeResp
{ holder, text, session_id?, mode?, source_record_iri?, extract?,
  images?: string[] /* http URLs or data: URLs; triggers OCR + vision */ }

// POST /memorize/batch → { results: MemorizeResp[] }
{ items: MemorizeReq[] }

// POST /ingest/preference → { record_iri, module_iri, anchored_to }
{ holder, key, value, session_id? }

// POST /recall → MemoryEvidenceBundle
{ holder, action?, query?, session_id?, subject?, predicate?,
  module_iris?, lens_name?, as_of_tx?, polarity?, limit?,
  permitted_only? }

// POST $DONTOSRV/sources/register → { document_id, iri, policy_iri }
{ iri, source_kind, policy_iri, media_type?, label?, source_url? }

// POST $DONTOSRV/documents/revision → { revision_id }
{ document_id, body?, parser_version? }
```

Full schemas live in [`/openapi.json`](/openapi.json); deep dive in
[`/agent.md`](/agent.md).

---

*donto-memory v0.1.0 · contract `0.1.0-m10` · Apache-2.0 OR MIT*
