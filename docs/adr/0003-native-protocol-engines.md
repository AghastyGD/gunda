# Implement Native Protocol Engines over Shared HTTP Transport

Status: Accepted

## Context

Gunda treats ordinary files and web media as first-class downloads. Direct HTTP
files, HLS playlists, and future DASH manifests have different inspection,
scheduling, resume, and finalization behavior, while all can require the same
basic HTTP capabilities.

## Decision

Gunda will implement native engines for supported resource types. Engines own
protocol inspection, execution, protocol-specific recovery data, and
finalization. They report results to the application layer and do not mutate
persistent jobs directly.

Low-level HTTP operations form a reusable transport shared by the direct HTTP
file engine and streaming engines. The direct file algorithm is not the shared
transport.

The exact engine trait and registration mechanism are deliberately deferred
until the direct HTTP implementation and HLS requirements have both informed the
interface. HLS is the first planned streaming engine. DASH design follows after
that boundary has evidence from two implemented resource types.

## Alternatives considered

- Delegating downloads to a general external tool would also delegate core
  persistence, retry, progress, and request-context behavior.
- One protocol-agnostic transfer algorithm would hide differences required for
  correct inspection, scheduling, resume, and finalization.
- Independent HTTP clients in each engine would duplicate redirect, header,
  range, and secret-handling policy.

## Consequences

- Protocol-specific models stay outside the generic core.
- HTTP request policy and secret handling can be enforced consistently across
  engines.
- Direct HTTP correctness and recovery precede range acceleration.
- Native HLS requires parser, scheduler, persistence, and finalization work owned
  by Gunda.
- External tools may later assist with a narrowly defined finalization step, but
  they do not become the authoritative download engine.
- Website-specific extractors are optional integration work, not the foundation
  of the architecture.
