<!-- rustcor-ai-rails:start -->
<!-- rustcor-ai-rails:v1 -->
# AI entry contract

This is the first instruction file for AI work in **rustcor/RFS**. Read it before planning or editing. Then read the README, the nearest nested `AGENTS.md`, relevant architecture/decision documents, and the CI workflows that govern the area being changed.

## Repository profile

- Build surfaces detected: Cargo.toml, fuzz/Cargo.toml, rfs-fuse/Cargo.toml
- Gitea workflow authority: ci.yml, release.yml
- Primary risk focus: persisted data, memory/device safety, recovery, and target-specific behavior

If a listed surface is generated, vendored, upstream-owned, platform-specific, or only a scaffold, determine that from repository evidence before changing it. Never assume every manifest belongs to one build.

## Engineering rails

### Architecture

- Keep one coherent responsibility per module. Do not combine transport, authorization, persistence, orchestration, rendering, and platform execution merely because an existing file already handles the feature.
- Treat 800 production lines as a review signal. At 1,500 production lines, do not add another responsibility without explicit justification. Prefer extracting a coherent seam, while keeping the requested change scoped.
- Reuse typed domain concepts; do not create parallel stringly typed status, phase, action, or error vocabularies.
- Keep adapters thin and business rules independently exercisable. Do not perform unrelated rewrites.

### Boundary safety

- Treat disk formats, network/IPC messages, environment and CLI input, webhooks, database rows, hardware responses, and model/tool output as untrusted.
- Validate lengths, tags, versions, ranges, identities, and state transitions. Unknown variants fail explicitly.
- Do not use panic paths, unchecked indexing, `unwrap`, or `expect` on externally influenced production data. Return typed, contextual errors.
- Keep blocking filesystem/process/device work off async executor threads unless a dedicated blocking contract exists.
- Security-sensitive work must consider replay, concurrency, idempotency, authorization scope, secret disclosure, downgrade behavior, and auditability. Document every unsafe invariant adjacent to the unsafe block.

### Evidence-driven tests

- Each test protects a named invariant, failure mechanism, compatibility contract, or regression.
- Prefer property, state-machine, fuzz, mutation, differential, and boundary tests when they expose more behavior than example enumeration.
- Avoid broad snapshots or exhaustive response/presentation assertions unless that shape is the supported contract.
- Coverage percentage and test LOC are diagnostic data, not targets. Never weaken, skip, or delete a meaningful test to make a change pass.

### Verification and claims

- Existing CI and documented workflows are the executable source of truth. Establish a relevant baseline when practical; run narrow checks while iterating, then applicable format, lint, build, and test gates.
- Never invent a command or claim an unrun check passed. Report platform/dependency blockers precisely.
- Use maturity terms literally: prototype, tested, production-hardened, and formally verified are distinct evidence levels.
- Completion reports name changed behavior, protected invariants, commands run with results, and remaining risks or unverified surfaces.

Repository-specific instructions below may strengthen this contract. They must not silently weaken it.
<!-- rustcor-ai-rails:end -->
