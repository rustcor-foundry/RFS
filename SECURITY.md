# Security Policy

## Reporting a vulnerability

If you believe you have found a security vulnerability in RFS, please **do
not** open a public issue. Instead:

- **Preferred:** use GitHub's [private vulnerability reporting][pvr] on this
  repository. It routes the report to the maintainer and opens a private
  advisory.
- **Alternative:** email the maintainer contact listed in `README.md`.

[pvr]: https://docs.github.com/en/code-security/security-advisories/guidance-on-reporting-and-writing-information-about-vulnerabilities/privately-reporting-a-security-vulnerability

Please include what you observed, what you expected, how to reproduce it, and
any disclosure deadline you are working to.

## What to expect

RustCor is a small operation and RFS is maintained alongside other work. The
commitments below are deliberately ones that can actually be met rather than
ones that sound reassuring:

- **Acknowledgement — target 14 days.** A human will confirm receipt.
- **Assessment — target 60 days.** A human will judge scope and severity and
  tell you the intended path, including "we are not going to fix this" if that
  is the answer.
- **Disclosure — coordinated, 90 days by default.** We aim to publish alongside
  a fix. If a fix is not ready we will say so rather than let the window lapse
  silently.
- **Credit** in the advisory unless you prefer otherwise.

If you need a faster response than that, say so in the report and we will do
what we can. If a deadline matters to you, tell us up front.

There is no bug bounty.

## Scope

Anything that lets an attacker cross a trust boundary — read data they should
not, execute code they should not, or escalate what they can do — is in scope.

This project is unaudited and has had no external security review.

Vulnerabilities in upstream dependencies should generally go to that project
first. If the issue is how RFS *uses* a dependency, it belongs here.
