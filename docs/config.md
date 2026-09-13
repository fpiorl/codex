# Configuration

For basic configuration instructions, see [this documentation](https://developers.openai.com/codex/config-basic).

For advanced configuration instructions, see [this documentation](https://developers.openai.com/codex/config-advanced).

For a full configuration reference, see [this documentation](https://developers.openai.com/codex/config-reference).

## Lifecycle hooks

Admins can set top-level `allow_managed_hooks_only = true` in
`requirements.toml` to ignore user, project, and session hook configs while
still allowing managed hooks from requirements and managed config layers. This
setting is only supported in `requirements.toml`; putting it in `config.toml`
does not enable managed-hooks-only mode.

## Privacy filter

The `[privacy]` table defines a bidirectional substitution layer that sits at
the model API boundary. Every string sent to the model (instructions, your
messages, tool definitions, command output, file contents, history) has each
rule's `real` value replaced by its `placeholder`. Every string coming back
from the model (assistant text, reasoning, tool call arguments such as shell
commands or patches) has placeholders mapped back to the real values before
anything else in Codex sees them. The model therefore never observes the real
value, while you, the shell and the filesystem never observe the placeholder.

```toml
[privacy]
enabled = true # optional; defaults to true when rules are present

[[privacy.rules]]
real = "acme.com"
placeholder = "company-a.example"

[[privacy.rules]]
real = "Acme Corp"
placeholder = "Company A"
```

Notes:

- Matching is case-insensitive for ASCII and preserves the case shape of the
  matched text (`ACME.COM` becomes `COMPANY-A.EXAMPLE`).
- Matching is on substrings, so a rule for `acme.com` also covers
  `api.acme.com` and `https://acme.com/login`.
- Rules are matched longest-first in a single pass; a replacement is never
  re-scanned against other rules.
- Pick placeholders the model is likely to echo verbatim (a fake domain, a
  short capitalised name). Avoid placeholders that also appear in your
  code or docs for unrelated reasons.
- The substitution is deterministic, so prompt caching keeps working.
