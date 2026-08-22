# Support

RFS is a copy-on-write, log-structured, self-healing filesystem engine. It ships as open source **without a support promise**.

This is a small operation. Public issues are read and appreciated, but response
is best-effort and can be slow. Treat RFS as software you are willing to
operate and debug yourself.

## Maturity

See the README for what is actually implemented and how far it has been
exercised. Where the README is unclear, assume a capability is less proven than
you would like and verify it yourself before depending on it.

Nothing here is feature-gated, license-keyed, or held back for a paid tier.
Where something is missing, it is unfinished — not withheld.

## Getting help

- **Bugs and regressions** — open an issue with the detail below.
- **Questions and design discussion** — open an issue; expect a slower reply.
- **Security issues** — please do not open a public issue. Contact RustCor
  directly so a fix can land before the details are public.

## Reporting issues

Include:

- how the filesystem was created and mounted
- workload that triggered it
- whether it reproduces on a fresh image
- toolchain version and commit
- what you expected, what happened, and the exact steps to reproduce
- logs or backtraces, as text rather than screenshots where possible
