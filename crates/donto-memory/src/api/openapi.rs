//! Hand-written OpenAPI 3.1 spec for donto-memory.
//!
//! Served at `GET /openapi.json`. A Swagger UI page at `GET /docs`
//! loads this and renders it interactively.
//!
//! For v0.1 this is hand-maintained alongside the route handlers. A
//! later refactor will derive it from `utoipa` annotations on the
//! handler signatures.

use serde_json::{json, Value};

pub fn document() -> Value {
    json!({
        "openapi": "3.1.0",
        "info": {
            "title": "donto-memory",
            "version": env!("CARGO_PKG_VERSION"),
            "description": concat!(
                "Agentic-memory runtime that runs on top of the donto ",
                "evidence substrate.\n\n",
                "Memory content lives as evidence-anchored claims in donto.",
                " donto-memory adds:\n",
                "- a *module* abstraction (episodic / semantic-claim / preference);\n",
                "- a *hot path* that fuses module outputs into a Memory Evidence Bundle;\n",
                "- a *sleep path* that reconsolidates via append-only DontoDelta ops.\n\n",
                "See README at https://github.com/thomasdavis/donto-memory."
            ),
            "license": {
                "name": "Apache-2.0 OR MIT",
            },
            "contact": {
                "url": "https://github.com/thomasdavis/donto-memory",
            },
        },
        "servers": [
            { "url": "https://memories.apexpots.com",     "description": "production" },
            { "url": "http://localhost:7900",             "description": "local" }
        ],
        "tags": [
            { "name": "system",     "description": "Health / version / substrate handshake." },
            { "name": "modules",    "description": "Memory module discovery." },
            { "name": "ingest",     "description": "Write a unit of memory into a module." },
            { "name": "recall",     "description": "Read a Memory Evidence Bundle." },
            { "name": "reconsolidate", "description": "Sleep-path queue management." }
        ],
        "paths": {
            "/": {
                "get": {
                    "tags": ["system"],
                    "summary": "Service summary + endpoint list",
                    "responses": { "200": { "description": "OK" } }
                }
            },
            "/health": {
                "get": {
                    "tags": ["system"],
                    "summary": "Liveness probe — returns `{status: ok}`.",
                    "responses": { "200": { "description": "OK" } }
                }
            },
            "/version": {
                "get": {
                    "tags": ["system"],
                    "summary": "Service version + substrate contract floor",
                    "responses": { "200": { "description": "OK" } }
                }
            },
            "/substrate": {
                "get": {
                    "tags": ["system"],
                    "summary": "Echo dontosrv /discovery/contract-version + /discovery/substrate-health",
                    "description":
                        "Verifies donto-memory is bound to the substrate the operator expects. \
                         No state mutated.",
                    "responses": { "200": { "description": "OK" } }
                }
            },
            "/modules": {
                "get": {
                    "tags": ["modules"],
                    "summary": "List registered modules (runtime + DB).",
                    "responses": { "200": { "description": "OK" } }
                }
            },
            "/ingest/{module_iri}": {
                "post": {
                    "tags": ["ingest"],
                    "summary": "Ingest into a module.",
                    "description":
                        "`module_iri` is one of the short names (`episodic`, `semantic-claim`, \
                         `preference`) or the full IRI (`mem:module/episodic` etc.). The request \
                         body's required fields depend on the module:\n\n\
                         * **episodic**: `holder`, `session_id`, `text`.\n\
                         * **semantic-claim**: `holder`, `subject`, `predicate`, and exactly one of \
                           `object_iri` / `object_lit`.\n\
                         * **preference**: `holder`, `key`, `value`.\n",
                    "parameters": [
                        {
                            "name": "module_iri",
                            "in": "path",
                            "required": true,
                            "schema": { "type": "string" },
                            "examples": {
                                "episodic":      { "value": "episodic" },
                                "semantic":      { "value": "semantic-claim" },
                                "preference":    { "value": "preference" },
                                "full_iri":      { "value": "mem:module/episodic" }
                            }
                        }
                    ],
                    "requestBody": {
                        "required": true,
                        "content": {
                            "application/json": {
                                "schema": { "$ref": "#/components/schemas/IngestInput" },
                                "examples": {
                                    "episodic_basic": {
                                        "summary": "Episodic chunk",
                                        "value": {
                                            "holder": "agent:ajax",
                                            "session_id": "s-2026-05-28",
                                            "text": "I met Annie Davis at the Cooktown Festival in 1979.",
                                            "modality": "model_output"
                                        }
                                    },
                                    "semantic_basic": {
                                        "summary": "Semantic claim with IRI object",
                                        "value": {
                                            "holder": "agent:ajax",
                                            "subject": "ex:annie-davis",
                                            "predicate": "ex:metAt",
                                            "object_iri": "ex:cooktown-festival-1979"
                                        }
                                    },
                                    "preference_basic": {
                                        "summary": "Preference (will supersede prior)",
                                        "value": {
                                            "holder": "agent:ajax",
                                            "key": "preferred_language",
                                            "value": "en"
                                        }
                                    }
                                }
                            }
                        }
                    },
                    "responses": {
                        "200": { "description": "Record created" },
                        "400": { "description": "Invalid input" },
                        "404": { "description": "Unknown module" }
                    }
                }
            },
            "/memorize": {
                "post": {
                    "tags": ["ingest"],
                    "summary": "Save memory — episodic chunk + LLM-extracted semantic claims",
                    "description":
                        "The agent-facing 'save memory' entrypoint. Always writes the raw \
                         text as an episodic chunk. If an LLM is configured (env \
                         `DONTO_MEMORY_LLM_BASE_URL` + `DONTO_MEMORY_LLM_API_KEY`), also \
                         calls the LLM to extract ontological statements about the text — \
                         each becomes a semantic-claim record with `source_record_iri` \
                         pointing back at the episodic chunk. Without an LLM configured, \
                         episodic-only with a warning.",
                    "requestBody": {
                        "required": true,
                        "content": {
                            "application/json": {
                                "schema": {
                                    "type": "object",
                                    "required": ["holder"],
                                    "properties": {
                                        "holder":     { "type": "string", "description": "Agent IRI (e.g. agent:my-bot)." },
                                        "session_id": { "type": "string", "nullable": true, "description": "Optional scope. Conventional shapes: `discord:user:<id>`, `conversation:<id>`. Recall can filter on this." },
                                        "text":       { "type": "string", "description": "The memory to store. Required unless `images` is non-empty (in which case OCR populates text)." },
                                        "modality":   { "type": "string", "default": "model_output", "description": "How this chunk came to exist. Values: model_output | descriptive | oral_history | community_protocol | inferred | reconstructed | elicited | experimental_result | clinical_observation." },
                                        "extract":    { "type": "boolean", "default": true,
                                                         "description": "Set false to skip LLM extraction and store episodic only." },
                                        "mode":       { "type": "string", "enum": ["single", "exhaustive", "multi", "apertures"],
                                                         "description": "Extraction mode. `single` = one LLM call (~30-100 facts, ~30-100s). `exhaustive` = five parallel apertures (~80-250 facts, ~60-180s, ~5× tokens). Defaults to DONTO_MEMORY_EXTRACT_MODE on the runtime." },
                                        "images":     { "type": "array",
                                                         "items": { "type": "string" },
                                                         "description": "Optional images. Each entry is an http(s) URL the LLM provider can fetch OR a `data:image/...;base64,…` data URL. When non-empty: (1) the extractor switches to OpenAI multimodal message format and uses `DONTO_MEMORY_LLM_VISION_MODEL` (currently `openai/gpt-4o-mini` in production), (2) an OCR pre-pass transcribes any visible text and prepends it to the episodic chunk as `[OCR text from image #N]\\n<transcript>` blocks — visible labels in screenshots/signs/captions become searchable via /recall query=. Disable OCR via DONTO_MEMORY_OCR_ENABLED=false." }
                                    }
                                },
                                "examples": {
                                    "basic": {
                                        "summary": "Plain text memorize",
                                        "value": {
                                            "holder": "agent:my-bot",
                                            "session_id": "discord:user:12345",
                                            "text": "I met Annie Davis at the Cooktown Festival in 1979."
                                        }
                                    },
                                    "image_url": {
                                        "summary": "Image-only with OCR + extraction",
                                        "value": {
                                            "holder": "agent:my-bot",
                                            "session_id": "screenshots",
                                            "text": "",
                                            "mode": "single",
                                            "images": ["https://picsum.photos/seed/example/512/512"]
                                        }
                                    },
                                    "data_url": {
                                        "summary": "Base64 inline image",
                                        "value": {
                                            "holder": "agent:my-bot",
                                            "session_id": "uploads",
                                            "text": "Screenshot from a chat",
                                            "mode": "single",
                                            "images": ["data:image/png;base64,iVBORw0K…"]
                                        }
                                    }
                                }
                            }
                        }
                    },
                    "responses": {
                        "200": { "description": "Episodic record + (optional) semantic claim records" },
                        "400": { "description": "Invalid input" }
                    }
                }
            },
            "/memorize/batch": {
                "post": {
                    "tags": ["ingest"],
                    "summary": "Batch memorize",
                    "description":
                        "Same flow as /memorize for each item in `items[]`. Per-item failures \
                         do not abort the rest; each result carries either the response or an \
                         error.",
                    "requestBody": {
                        "required": true,
                        "content": {
                            "application/json": {
                                "schema": {
                                    "type": "object",
                                    "required": ["items"],
                                    "properties": {
                                        "items": {
                                            "type": "array",
                                            "items": { "$ref": "#/components/schemas/IngestInput" }
                                        }
                                    }
                                }
                            }
                        }
                    },
                    "responses": { "200": { "description": "Per-item results" } }
                }
            },
            "/recall": {
                "post": {
                    "tags": ["recall"],
                    "summary": "Memory Evidence Bundle",
                    "description":
                        "Composes a single-call response across every enabled module. Substrate \
                         /recall is consulted for the policy gate + identity-lens resolution; \
                         donto-memory adds RRF fusion across modules and bookkeeping (access \
                         events + reconsolidation enqueue).",
                    "requestBody": {
                        "required": true,
                        "content": {
                            "application/json": {
                                "schema": { "$ref": "#/components/schemas/RecallQuery" },
                                "examples": {
                                    "basic": {
                                        "summary": "Permission-gated recall",
                                        "value": {
                                            "holder": "agent:ajax",
                                            "action": "read_content",
                                            "query": "Annie Davis",
                                            "session_id": "s-2026-05-28",
                                            "limit": 20,
                                            "permitted_only": true
                                        }
                                    },
                                    "as_of": {
                                        "summary": "Bitemporal time-travel",
                                        "value": {
                                            "holder": "agent:ajax",
                                            "action": "read_metadata",
                                            "as_of_tx": "2026-05-01T00:00:00Z",
                                            "limit": 50
                                        }
                                    }
                                }
                            }
                        }
                    },
                    "responses": {
                        "200": {
                            "description": "Memory Evidence Bundle",
                            "content": {
                                "application/json": {
                                    "schema": { "$ref": "#/components/schemas/MemoryEvidenceBundle" }
                                }
                            }
                        }
                    }
                }
            },
            "/reconsolidate/enqueue": {
                "post": {
                    "tags": ["reconsolidate"],
                    "summary": "Manually enqueue a record for sleep-path reconsolidation",
                    "requestBody": {
                        "required": true,
                        "content": {
                            "application/json": {
                                "schema": {
                                    "type": "object",
                                    "required": ["record_id"],
                                    "properties": {
                                        "record_id": { "type": "string", "format": "uuid" },
                                        "reason": { "type": "string", "default": "explicit" },
                                        "priority": { "type": "number", "default": 0.0 }
                                    }
                                }
                            }
                        }
                    },
                    "responses": {
                        "200": { "description": "Enqueued" },
                        "404": { "description": "Record not found" }
                    }
                }
            },
            "/reconsolidate/queue": {
                "get": {
                    "tags": ["reconsolidate"],
                    "summary": "List head-of-queue items (unfinished work)",
                    "responses": { "200": { "description": "OK" } }
                }
            },
            "/openapi.json": {
                "get": {
                    "tags": ["system"],
                    "summary": "This OpenAPI 3.1 document",
                    "responses": { "200": { "description": "OK" } }
                }
            },
            "/docs": {
                "get": {
                    "tags": ["system"],
                    "summary": "Swagger UI rendering of /openapi.json",
                    "responses": { "200": { "description": "HTML" } }
                }
            },
            "/agent.md": {
                "get": {
                    "tags": ["system"],
                    "summary": "Agent-facing markdown guide",
                    "description": "Comprehensive contract guide aimed at AI agents implementing memory storage and recall. Served as text/markdown.",
                    "responses": { "200": { "description": "markdown", "content": { "text/markdown": {} } } }
                }
            },
            "/llms.txt": {
                "get": {
                    "tags": ["system"],
                    "summary": "Same content as /agent.md, plain text",
                    "description": "Follows the llms.txt convention for AI-agent site documentation. Identical body to /agent.md but served as text/plain.",
                    "responses": { "200": { "description": "plain text", "content": { "text/plain": {} } } }
                }
            },
            "/jobs": {
                "get": {
                    "tags": ["system"],
                    "summary": "HTML observability page (job history)",
                    "description": "Browseable list of every /memorize, /recall, and /ingest call with full request/response bodies. Filter via the `endpoint` and `holder` query parameters.",
                    "parameters": [
                        { "name": "endpoint", "in": "query", "schema": {"type": "string"}, "description": "Substring filter on the endpoint label." },
                        { "name": "holder", "in": "query", "schema": {"type": "string"}, "description": "Exact-match filter on the holder IRI." },
                        { "name": "limit", "in": "query", "schema": {"type": "integer", "default": 100}, "description": "Max rows to return (1..1000)." }
                    ],
                    "responses": { "200": { "description": "HTML" } }
                }
            },
            "/jobs/list.json": {
                "get": {
                    "tags": ["system"],
                    "summary": "JSON list view of recent jobs",
                    "description": "Programmatic equivalent of GET /jobs. Same query params; returns `{count, jobs[]}`.",
                    "parameters": [
                        { "name": "endpoint", "in": "query", "schema": {"type": "string"} },
                        { "name": "holder", "in": "query", "schema": {"type": "string"} },
                        { "name": "limit", "in": "query", "schema": {"type": "integer", "default": 100} }
                    ],
                    "responses": {
                        "200": {
                            "description": "OK",
                            "content": {
                                "application/json": {
                                    "schema": {
                                        "type": "object",
                                        "properties": {
                                            "count": {"type": "integer"},
                                            "jobs": {"type": "array", "items": {"$ref": "#/components/schemas/JobRow"}}
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            },
            "/jobs/{id}": {
                "get": {
                    "tags": ["system"],
                    "summary": "HTML detail for a single job",
                    "description": "Per-job page showing the full request/response. For /memorize jobs, also renders every extracted fact as a sortable table.",
                    "parameters": [
                        { "name": "id", "in": "path", "required": true, "schema": {"type": "string", "format": "uuid"} }
                    ],
                    "responses": {
                        "200": { "description": "HTML" },
                        "400": { "description": "Invalid UUID" },
                        "404": { "description": "Job not found" }
                    }
                }
            },
            "/jobs/{id}/raw": {
                "get": {
                    "tags": ["system"],
                    "summary": "Raw JSON for a single job",
                    "parameters": [
                        { "name": "id", "in": "path", "required": true, "schema": {"type": "string", "format": "uuid"} }
                    ],
                    "responses": {
                        "200": {
                            "description": "OK",
                            "content": {
                                "application/json": {
                                    "schema": {"$ref": "#/components/schemas/JobDetail"}
                                }
                            }
                        },
                        "404": { "description": "Job not found" }
                    }
                }
            }
        },
        "components": {
            "schemas": {
                "IngestInput": {
                    "type": "object",
                    "required": ["holder"],
                    "properties": {
                        "holder":        { "type": "string", "description": "Agent IRI (e.g. agent:ajax)" },
                        "session_id":    { "type": "string", "nullable": true },
                        "text":          { "type": "string", "default": "" },
                        "modality":      { "type": "string", "default": "model_output",
                                            "enum": [
                                                "descriptive", "prescriptive", "reconstructed",
                                                "inferred", "elicited", "corpus_observed",
                                                "typological_summary", "experimental_result",
                                                "clinical_observation", "legal_holding",
                                                "archival_metadata", "oral_history",
                                                "community_protocol", "model_output", "other"
                                            ]},
                        "subject":       { "type": "string", "nullable": true, "description": "semantic-claim only" },
                        "predicate":     { "type": "string", "nullable": true, "description": "semantic-claim only" },
                        "object_iri":    { "type": "string", "nullable": true, "description": "semantic-claim only" },
                        "object_lit":    { "type": "object", "nullable": true, "description": "semantic-claim only" },
                        "key":           { "type": "string", "nullable": true, "description": "preference only" },
                        "value":         { "type": "string", "nullable": true, "description": "preference only" }
                    }
                },
                "RecallQuery": {
                    "type": "object",
                    "required": ["holder"],
                    "properties": {
                        "holder":         { "type": "string" },
                        "action":         {
                            "type": "string",
                            "default": "read_content",
                            "enum": [
                                "read_metadata", "read_content", "quote", "view_anchor_location",
                                "derive_claims", "derive_embeddings", "translate", "summarize",
                                "export_claims", "export_sources", "export_anchors",
                                "train_model", "publish_release", "share_with_third_party",
                                "federated_query", "request_deletion"
                            ]
                        },
                        "query":          { "type": "string", "nullable": true },
                        "session_id":     { "type": "string", "nullable": true },
                        "subject":        { "type": "string", "nullable": true },
                        "predicate":      { "type": "string", "nullable": true },
                        "object_iri":     { "type": "string", "nullable": true },
                        "module_iris":    { "type": "array", "items": { "type": "string" }, "nullable": true },
                        "lens_name":      { "type": "string", "nullable": true,
                                            "description": "Identity hypothesis name (e.g. 'strict_identity_v1')" },
                        "as_of_tx":       { "type": "string", "format": "date-time", "nullable": true,
                                            "description": "Bitemporal time travel — what did we know on this date?" },
                        "polarity":       { "type": "string", "default": "asserted" },
                        "min_maturity":   { "type": "integer", "default": 0, "minimum": 0, "maximum": 4 },
                        "limit":          { "type": "integer", "default": 50, "minimum": 1 },
                        "permitted_only": { "type": "boolean", "default": true }
                    }
                },
                "MemoryEvidenceBundle": {
                    "type": "object",
                    "properties": {
                        "holder":        { "type": "string" },
                        "action":        { "type": "string" },
                        "lens":          { "type": "string", "nullable": true },
                        "as_of":         { "type": "string", "format": "date-time", "nullable": true },
                        "rows":          { "type": "array", "items": { "$ref": "#/components/schemas/RecallRow" } },
                        "row_count":     { "type": "integer" },
                        "modules_used":  { "type": "array", "items": { "type": "string" } },
                        "policy_report": { "type": "object" }
                    }
                },
                "RecallRow": {
                    "type": "object",
                    "properties": {
                        "statement_id":      { "type": "string", "format": "uuid" },
                        "subject":           { "type": "string" },
                        "predicate":         { "type": "string" },
                        "object_iri":        { "type": "string", "nullable": true },
                        "object_lit":        { "type": "object", "nullable": true },
                        "context":           { "type": "string" },
                        "polarity":          { "type": "string" },
                        "maturity":          { "type": "integer" },
                        "valid_lo":          { "type": "string", "format": "date", "nullable": true },
                        "valid_hi":          { "type": "string", "format": "date", "nullable": true },
                        "tx_lo":             { "type": "string", "format": "date-time" },
                        "tx_hi":             { "type": "string", "format": "date-time", "nullable": true },
                        "resolved_subject":  { "type": "string", "nullable": true },
                        "resolved_object":   { "type": "string", "nullable": true },
                        "effective_actions": { "type": "object", "additionalProperties": { "type": "boolean" } },
                        "action_allowed":    { "type": "boolean" },
                        "record_iri":        { "type": "string", "nullable": true },
                        "module_iri":        { "type": "string", "nullable": true },
                        "score":             { "type": "number", "nullable": true },
                        "rank":              { "type": "integer", "nullable": true }
                    }
                },
                "JobRow": {
                    "type": "object",
                    "description": "One audit-log row as returned by /jobs/list.json.",
                    "properties": {
                        "job_id":          { "type": "string", "format": "uuid" },
                        "created_at":      { "type": "string", "format": "date-time" },
                        "endpoint":        { "type": "string", "example": "POST /memorize" },
                        "holder":          { "type": "string", "nullable": true },
                        "session_id":      { "type": "string", "nullable": true },
                        "status_code":     { "type": "integer", "example": 200 },
                        "elapsed_ms":      { "type": "integer" },
                        "facts_extracted": { "type": "integer", "nullable": true },
                        "facts_ingested":  { "type": "integer", "nullable": true },
                        "rows_returned":   { "type": "integer", "nullable": true },
                        "model":           { "type": "string", "nullable": true },
                        "total_tokens":    { "type": "integer", "nullable": true },
                        "error":           { "type": "string", "nullable": true }
                    }
                },
                "JobDetail": {
                    "allOf": [
                        { "$ref": "#/components/schemas/JobRow" },
                        {
                            "type": "object",
                            "properties": {
                                "request":           { "type": "object", "description": "Full request body that was POSTed." },
                                "response":         { "type": "object", "description": "Full response body returned to the caller." },
                                "prompt_tokens":     { "type": "integer", "nullable": true },
                                "completion_tokens": { "type": "integer", "nullable": true }
                            }
                        }
                    ]
                }
            }
        }
    })
}
