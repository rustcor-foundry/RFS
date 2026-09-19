# Contributing

RFS is a copy-on-write, log-structured, self-healing filesystem engine.

Thanks for looking. A few things worth knowing before you spend time.

## Before larger changes

Open an issue first. This project moves in bursts and it would be a shame for
you to spend an evening on something that collides with work already in flight,
or that does not fit the direction.

Small fixes — documentation, obvious bugs, tests for existing behaviour — do not
need that ceremony. Just send them.

## Contributor terms

RFS is permissively licensed, so there is no contributor licence agreement to
sign. By sending a change you are offering it under the same terms as the rest
of the repository.

Please make sure you actually have the right to contribute what you are sending
— in particular, that it is not lifted from a differently-licensed project.

## AI-authored contributions

This is worth being explicit about, because most of this codebase was written
with AI assistance and pretending otherwise would be silly.

- **AI-generated contributions are not categorically different.** Whether a
  change was written by a human, an AI, or the two together, the same review
  applies. It is judged on what it does, not who emitted it.
- **`Co-Authored-By` trailers are required** for AI-authored content. The commit
  history carries these throughout. This is provenance, not credit-claiming.
- **Review focuses on intent and invariants.** The interesting questions are
  what the change is trying to guarantee and whether it actually does — not
  whether every line was typed by a person.
- **Design decisions stay human-anchored.** An agent can draft a proposal; the
  decision to adopt it is a human one.

## Style

Match the surrounding code. Run whatever the repository's CI runs before
sending — formatting and lint failures are the most common reason a change sits
unreviewed.

## Security

Do not report security issues through public issues or pull requests. See
[SECURITY.md](SECURITY.md).
