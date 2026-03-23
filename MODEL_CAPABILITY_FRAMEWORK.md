# Model & Capability Framework

## Goals

- Support multiple API providers, not just OpenRouter
- Support many model providers behind a single API provider
- Let MAAT be parsimonious by default
- Let PHAROH use a cheaper/default model while MINIONs can escalate for harder work
- Allow whitelist/blacklist policy at multiple levels:
  - component
  - session
  - talent
  - skill
  - capability
  - task route

## The Four Spaces

### 1. Provider Space

Provider space is about **how we talk to models**.

Examples:
- OpenRouter
- OpenAI
- Anthropic via compatibility layer
- local OpenAI-compatible endpoint

Each provider entry should define:
- `id`
- API style
- base URL
- auth env var / secret key

This is intentionally separate from model choice.

### 2. Profile Space

Profile space is about **what a model is good for**.

A profile is MAAT's stable internal handle for a usable model configuration.

Examples:
- `cheap_chat`
- `tool_worker_fast`
- `planner_reasoning`
- `long_context_summarizer`

Each profile should map to:
- provider
- provider-native model id
- temperature / token defaults
- traits:
  - tool-calling
  - long-context
  - structured-output
  - reasoning
  - vision
  - fast-response
  - low-cost
- tiers:
  - cost
  - latency
  - reasoning

The point is that routing should target profiles or profile traits, not raw model strings.

### 3. Policy Space

Policy space is about **what is allowed or preferred in a given execution context**.

Policies should support:
- prefer list
- allow list
- deny list
- required traits
- max cost tier
- max latency tier
- minimum reasoning tier
- require tool-calling
- fallback profile

Policy scope examples:
- PHAROH primary chat
- planner
- summarizer
- named session default
- talent: `search`
- skill: `code-review`
- capability id
- capability tag: `email`, `filesystem`, `calendar`

### 4. Capability Space

Capability space is about **what the agent can do**.

Capabilities should carry:
- semantic description
- tags
- permissions
- cost hints
- model routing hints

The model hint should be advisory, not absolute, unless backed by an allow/deny policy.

## Recommended Routing Order

When VIZIER resolves a model for a step, it should merge constraints in this order:

1. Hard deny rules
2. Hard allow rules
3. Capability-specific policy
4. Task-level policy
5. Session-level default policy
6. Global routing defaults
7. Fallback profile

That keeps the system predictable and explainable.

## Parsimony Model

Parsimony should be a default routing principle, not just a config preference.

Recommended baseline:
- PHAROH primary: cheap/fast conversational profile
- planner: medium or high reasoning profile
- summarizer/compactor: cheap long-context profile
- tool-worker MINIONs: fast tool-capable profile
- high-complexity reasoning MINIONs: premium reasoning profile only when needed

The idea is:
- cheap model for conversation and orchestration
- better model only for steps that justify the cost

## Capability Authoring Guidance

Talents and skills should eventually publish capability cards with:
- domain tags
- expected latency/cost
- permission requirements
- preferred model traits
- optional allow/deny profile constraints

Examples:

### Web search talent
- tags: `search`, `web`
- preferred model traits: `fast-response`, `tool-calling`
- avoid premium reasoning by default

### File analysis skill
- tags: `filesystem`, `code`
- preferred model traits: `structured-output`
- allow escalation to stronger reasoning for synthesis steps

### Calendar write capability
- tags: `calendar`, `write`
- require tool-calling
- prefer reliable structured-output profile

## Near-Term Implementation Plan

### Step 1
- Add provider/profile/routing config structures
- Add core model policy types
- Add model-routing hints to capability cards

### Step 2
- Build an in-memory `ModelRegistry`
- Build an in-memory `CapabilityRegistry`

### Step 3
- Teach VIZIER to resolve models from policies and capability hints

### Step 4
- Teach planner to choose both:
  - capability
  - model

## What Not To Do

- Do not let MINION choose its own model ad hoc
- Do not route directly on raw provider model strings everywhere
- Do not couple "cheap vs expensive" only to PHAROH vs MINION
- Do not make talent/skill authors encode provider-specific model names in code unless absolutely necessary

The stable abstraction should be:
- providers are transport
- profiles are reusable model identities
- policies are routing constraints
- capabilities provide routing hints
