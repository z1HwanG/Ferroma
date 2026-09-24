# Contributing to Ferroma

Ferroma stores other people's mail. A change that looks small can lose someone's message, so the
bar is not "it compiles" — it is what happens on the bad day. Everything below follows from that.

## Before you start

* **[`AGENTS.md`](AGENTS.md) is the working agreement.** It has the repository layout, the two
  environment quirks, and the conventions that are not negotiable: no `unwrap()` on untrusted
  input, no `sqlx::query!` macros, rustls everywhere, every public item documented.
* **A behaviour change needs a test that would fail without it.** "It compiles" and "I tried it
  once" are not done; a test that passes either way is a comment with extra steps.
* **The documents are bilingual.** A change to a document under `docs/` is a change to its pair,
  and `node tools/check-zh.mjs` is what says so — it compares headings, code fences and the
  language of every cross-reference.

## The checks that have to pass

```bash
cargo test --workspace --offline      # unit, integration, and the end-to-end acceptance run
node tools/check-docs.mjs             # every link and anchor, and the en/zh pairs
node tools/check-zh.mjs               # the Chinese set: terminology, pairing, references
node tools/check-deploy.mjs           # the deployment artefacts, Dockerfile included
(cd web && node tools/check.mjs)      # module graph, ids, i18n coverage
(cd admin && node tools/check.mjs)
node tools/check-web.mjs              # cross-module link errors, and the app-local suites
```

The front-end checks are strict on purpose. They fail on an element id that no view defines, on a
`t('…')` string the Chinese catalog does not cover, and on a `fetch()` outside `shared/api.js` —
each of those has already shipped as a bug once.

## Commits and pull requests

* One change per commit, and the message says **why**. The diff already says what changed; what it
  cannot say is what the alternative was and why it lost.
* Subject line in the imperative, under about 72 characters.
* Reference an issue when there is one.

## Sign-off (DCO)

By contributing you agree to the [Developer Certificate of Origin](https://developercertificate.org/),
and you record it by signing your commits off:

```bash
git commit -s -m "imap: …"
```

which adds a line to the message:

```text
Signed-off-by: Your Name <you@example.com>
```

That is the whole claim: you wrote it, or you have the right to pass it on. There is no CLA.
Contributions come in under the project's own licence, [AGPL-3.0-only](LICENSE) — the same terms
everyone else receives it under.

## Security

Do not open an issue for a vulnerability that a reader could act on. `docs/security.md` describes
the threat model; report privately to the address in the repository's profile, and expect an
acknowledgement before a fix.
