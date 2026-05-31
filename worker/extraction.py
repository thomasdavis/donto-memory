"""OpenCode-agent memory extraction.

Turns a raw memory chunk into an exhaustive set of donto-shaped facts by
driving an OpenCodeAgent with a sophisticated extraction prompt. The agent
writes its JSON to a file and self-validates it (re-parsing + fixing until
valid), which is far more reliable than parsing a chat stream.

Multi-pass: each pass after the first is shown a compact list of the facts
found so far and asked for NEW angles only — the same prior-facts technique
the in-process deep extractor used, but each pass is a full agentic session.
Facts are content-key deduped across passes; passes stop early once a pass
adds nothing.

Output facts are normalised to donto-memory's ExtractedFact shape
(`object_iri` XOR `object_lit`) so they can be POSTed straight to
`/memorize {facts:[...]}`.
"""

from __future__ import annotations

import json
import logging
import re

from opencode_agent import OpenCodeAgent, AgentRun

logger = logging.getLogger("extraction")

INPUT_NAME = "input.txt"
OUTPUT_NAME = "facts.json"

# CURIE / IRI-ish object → object_iri; everything else → typed object_lit.
_CURIE = re.compile(r"^[A-Za-z][A-Za-z0-9_.-]*:[^\s/].*$")
_INT = re.compile(r"^-?\d+$")
_DEC = re.compile(r"^-?\d+\.\d+$")


def _build_prompt(prior_block: str | None, target: int) -> str:
    base = f"""You are an EXHAUSTIVE ontological knowledge-extraction engine for the donto memory substrate.

Read the file `{INPUT_NAME}` in the current directory. Decompose its content into the LARGEST justifiable set of discrete RDF-style facts. Cover every layer:
- surface claims actually stated
- entity type assertions (rdf:type) for every entity, including abstract/conceptual ones
- properties and attributes (including implied ones)
- presuppositions the text takes for granted
- inferences a reasonable reader would draw
- conceivable attributes given the entity types
- relationships, causal/dependency links, contrasts
- temporal and spatial anchors
- the speaker's evident expertise, preferences, dependencies, workflow, emotional state, substitutes-avoided
- domain knowledge implied (related tools, standards, formats, practitioners, ecosystem)
- metalinguistic facts about the utterance itself (mood, register, sentiment, speech act, politeness, implicature)

Be relentless — aim for at least {target} facts.

Use donto conventions for objects:
- a class/entity/concept object is a CURIE like `donto:Pandoc`, `rdf:type`, `ex:Format`
- a literal object is a plain value (string, number, or true/false)

Then you MUST persist and self-verify:
1. Write the JSON to `{OUTPUT_NAME}` with shape:
   {{"facts":[{{"subject":"...","predicate":"...","object":"...","confidence":0.0,"modality":"descriptive|inferred"}}]}}
2. Run: node -e "const f=JSON.parse(require('fs').readFileSync('{OUTPUT_NAME}','utf8'));if(!Array.isArray(f.facts))throw new Error('no facts array');console.log('VALID',f.facts.length)"
3. If that errors, FIX `{OUTPUT_NAME}` and repeat step 2 until it prints VALID.

Do NOT print the JSON to the console — write the file. Do NOT ask questions."""
    if prior_block:
        base += f"""

ALREADY EXTRACTED in previous passes (do NOT repeat these — find genuinely NEW facts, deeper inferences, finer properties, new entities/angles):
{prior_block}"""
    return base


def _format_prior(facts: list[dict], cap: int = 300) -> str:
    """Compact (s | p | o) list of the most recent facts for the next pass."""
    recent = facts[-cap:]
    lines = []
    for f in recent:
        obj = f.get("object_iri")
        if obj is None:
            lit = f.get("object_lit") or {}
            obj = lit.get("v", "—")
        lines.append(f"- {f.get('subject')} | {f.get('predicate')} | {obj}")
    return "\n".join(lines)


def _parse_facts(raw: str) -> list[dict]:
    """Robustly parse the {"facts":[...]} object the agent wrote. Tolerates
    markdown fences and trailing junk; salvages individual objects if the
    whole document doesn't parse."""
    if not raw or not raw.strip():
        return []
    s = raw.strip()
    # Strip markdown fences if present.
    if s.startswith("```"):
        s = re.sub(r"^```[a-zA-Z]*\n?", "", s)
        s = re.sub(r"\n?```\s*$", "", s)
    # Fast path: whole thing parses.
    try:
        obj = json.loads(s)
        if isinstance(obj, dict) and isinstance(obj.get("facts"), list):
            return obj["facts"]
        if isinstance(obj, list):
            return obj
    except Exception:
        pass
    # Locate the facts array and parse element-by-element (salvage).
    start = s.find('"facts"')
    if start == -1:
        return []
    bracket = s.find("[", start)
    if bracket == -1:
        return []
    facts: list[dict] = []
    depth = 0
    buf = ""
    for ch in s[bracket + 1:]:
        if ch == "{":
            depth += 1
        if depth > 0:
            buf += ch
        if ch == "}":
            depth -= 1
            if depth == 0 and buf:
                try:
                    facts.append(json.loads(buf))
                except Exception:
                    pass
                buf = ""
    return facts


def _normalize(f: dict) -> dict | None:
    """Map a {subject,predicate,object,confidence,modality} fact to donto's
    ExtractedFact shape (object_iri XOR object_lit). Returns None if unusable."""
    subj = (f.get("subject") or "").strip()
    pred = (f.get("predicate") or "").strip()
    if not subj or not pred:
        return None
    out: dict = {"subject": subj, "predicate": pred}

    # Object may already be split (object_iri/object_lit) or a bare "object".
    if f.get("object_iri"):
        out["object_iri"] = str(f["object_iri"]).strip()
    elif f.get("object_lit") is not None:
        out["object_lit"] = f["object_lit"]
    else:
        obj = f.get("object")
        if obj is None or (isinstance(obj, str) and not obj.strip()):
            return None
        if isinstance(obj, bool):
            out["object_lit"] = {"dt": "xsd:boolean", "v": obj}
        elif isinstance(obj, (int, float)):
            dt = "xsd:integer" if isinstance(obj, int) else "xsd:decimal"
            out["object_lit"] = {"dt": dt, "v": obj}
        else:
            o = str(obj).strip()
            low = o.lower()
            if low in ("true", "false"):
                out["object_lit"] = {"dt": "xsd:boolean", "v": low == "true"}
            elif _INT.match(o):
                out["object_lit"] = {"dt": "xsd:integer", "v": int(o)}
            elif _DEC.match(o):
                out["object_lit"] = {"dt": "xsd:decimal", "v": float(o)}
            elif _CURIE.match(o) and " " not in o:
                out["object_iri"] = o
            else:
                out["object_lit"] = {"dt": "xsd:string", "v": o}

    conf = f.get("confidence")
    if isinstance(conf, (int, float)):
        out["confidence"] = float(conf)
    mod = f.get("modality")
    if isinstance(mod, str) and mod:
        out["modality"] = mod
    return out


def _key(f: dict) -> str:
    obj = f.get("object_iri")
    if obj is None:
        obj = json.dumps(f.get("object_lit"), sort_keys=True)
    return f"{f['subject']}\x1f{f['predicate']}\x1f{obj}".lower()


def extract_facts(
    text: str,
    *,
    agent: OpenCodeAgent,
    passes: int = 1,
    target_per_pass: int = 400,
    timeout_per_pass: int = 780,
    model: str | None = None,
) -> dict:
    """Run N agentic extraction passes over `text`, deduping across passes.

    Returns {"facts": [normalised donto facts], "passes": [per-pass meta]}.
    """
    all_facts: list[dict] = []
    seen: set[str] = set()
    pass_meta: list[dict] = []

    for p in range(1, max(1, passes) + 1):
        prior = _format_prior(all_facts) if all_facts else None
        prompt = _build_prompt(prior, target_per_pass)
        run: AgentRun = agent.run(
            prompt,
            input_files={INPUT_NAME: text},
            output_files=[OUTPUT_NAME],
            timeout=timeout_per_pass,
            model=model,
        )
        raw_facts = _parse_facts(run.output_files.get(OUTPUT_NAME, ""))
        added = 0
        for rf in raw_facts:
            nf = _normalize(rf)
            if nf is None:
                continue
            k = _key(nf)
            if k in seen:
                continue
            seen.add(k)
            all_facts.append(nf)
            added += 1
        pass_meta.append({
            "pass": p,
            "raw": len(raw_facts),
            "new": added,
            "cumulative": len(all_facts),
            "elapsed_s": round(run.elapsed_s, 1),
            "ok": run.ok,
            "exit": run.exit_code,
        })
        logger.info(
            "extract pass %d/%d: raw=%d new=%d cumulative=%d elapsed=%.1fs ok=%s",
            p, passes, len(raw_facts), added, len(all_facts), run.elapsed_s, run.ok,
        )
        # Saturated or failed → stop early.
        if p > 1 and added == 0:
            break
        if not run.ok and not raw_facts:
            break

    return {"facts": all_facts, "passes": pass_meta}
