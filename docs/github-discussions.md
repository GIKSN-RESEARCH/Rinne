# 👋 Welcome to Rinne Discussions

**Rinne** is a local, open-source, terminal-first AI orchestration tool. You talk to one CLI; it plans your work into a graph and drives it across the AI coding CLIs and model APIs already on your machine, through a verifying generator–evaluator loop. No hosted component, no telemetry, no accounts.

This is the place to **ask questions, share what you built, propose ideas, and help each other**. New here? Start with the [README](https://github.com/GIKSN-RESEARCH/Rinne#readme).

## Where should I post?

| I want to… | Go to |
|---|---|
| Report a **bug** / something broken | [**Issues**](https://github.com/GIKSN-RESEARCH/Rinne/issues/new/choose) |
| Ask "**how do I…**" / get help | **Q&A** |
| Suggest a **feature / idea** | **Ideas** |
| Show off a **setup, workflow, or recipe** | **Show and tell** |
| Tips for a specific **harness or provider** | **Workers & providers** |
| Anything else | **General** |
| Report a **security vulnerability** | **Privately** — see the [Security policy](https://github.com/GIKSN-RESEARCH/Rinne/security/policy). Never in public. |

## 🔐 Never paste secrets

Rinne handles API keys and tokens. **Do not paste API keys, tokens, or full `connect` / `--key` commands** into any issue or discussion. Rinne redacts these in its own transcript and history, but copy-pasted shell output won't be. Replace any secret with `***` before posting — and if you've already exposed one, **rotate it immediately**.

## Asking a good question

Include:
- What you ran (prompt/command, secrets redacted) and expected vs. actual
- `rinne --version` and your OS + terminal
- Whether it's a **harness** (claude-code, codex, …) or **API** worker, and which model/provider
- `rinne doctor` output if it's about detection/auth (it never prints keys)

## Helpful links

- 📖 [README](https://github.com/GIKSN-RESEARCH/Rinne#readme) · ⚙️ [Configuration](https://github.com/GIKSN-RESEARCH/Rinne#configuration) · 🔑 [Secrets & keychain](https://github.com/GIKSN-RESEARCH/Rinne#secrets--auth)
- 🐛 [Open an issue](https://github.com/GIKSN-RESEARCH/Rinne/issues/new/choose)
- 📦 [crates.io/crates/rinne](https://crates.io/crates/rinne) · 📚 [docs.rs/rinne](https://docs.rs/rinne)

## Be kind

Assume good faith, keep it on-topic, and remember everyone here is a volunteer. 🦀

---

# 📌 Read this first: where bugs, questions, and ideas go

- **Found a bug?** → [Open an Issue](https://github.com/GIKSN-RESEARCH/Rinne/issues/new/choose). Bugs in Discussions get lost; Issues are tracked and closed.
- **Have a question / need help?** → **Q&A**.
- **Want a feature?** → **Ideas**. If maintainers agree, it becomes an Issue.
- **Security vulnerability?** → report **privately** via the [Security policy](https://github.com/GIKSN-RESEARCH/Rinne/security/policy). Never in public.

### ⚠️ Before you post
- **Redact secrets.** No API keys, tokens, or full `connect` / `--key` lines. Replace with `***`. If exposed, **rotate the key**.
- **Search first** — your question may already be answered.
- **One topic per thread.**

---

# 🚀 Rinne 0.1.2 is out

Highlights in this release:

- **Markdown rendering of model output** — headings, tables, lists, and code render in the terminal instead of raw text.
- **Reasoning / "thinking" display** — reasoning models stream their chain-of-thought as a dimmed block; collapse it with **Ctrl+O**.
- **Persistent prompt history** — ↑/↓ recall across sessions, with secret-bearing commands filtered out.
- **Fully dynamic API support** — connect any OpenAI-compatible provider with any base URL and any number of keys; tokens are stored in your OS keychain (set once and forget).

Install or update:

```bash
cargo install rinne --force
```

Full details in the [README](https://github.com/GIKSN-RESEARCH/Rinne#readme). Questions in **Q&A**, ideas in **Ideas**, bugs in [Issues](https://github.com/GIKSN-RESEARCH/Rinne/issues/new/choose).

---

# How do I add models from a provider that isn't in the built-in catalog?

Rinne works with **any** OpenAI-compatible API, not just the built-in providers. Use `connect` with `--base-url` to point a custom name at any host:

```bash
rinne connect <name> <API_KEY> --base-url <https-endpoint>/v1 --model <model-id>
```

Example — Cloudflare Workers AI (account-scoped endpoint):

```bash
rinne connect cloudflare <TOKEN> \
  --base-url https://api.cloudflare.com/client/v4/accounts/<ACCOUNT_ID>/ai/v1 \
  --model @cf/meta/llama-3.3-70b-instruct-fp8-fast
```

The key goes to your OS keychain (never the config file). Pass `--model` multiple times to set a cheap→strong ladder, and run `rinne models <name>` to list what your key can reach.

---

# Post your worker pool 🛠️

Share how you've got Rinne set up — drop a reply with:

- Your **generator** and **evaluator** mix (which harnesses / API models)
- Your **conductor** backend (cloudflare / groq / local / harness)
- Any config worth stealing (scrub your keys → `***`)

Curious what combinations people are running.

---

## Category descriptions

**📣 Announcements** — Releases, roadmap updates, and project news. Maintainers post; everyone can react and reply.

**🙏 Q&A** — Stuck? Ask here. "How do I connect a custom provider?", "Why did it route to a harness?" Mark an answer when it helps.

**💡 Ideas** — Feature requests and design ideas. Describe the problem first, then your proposed solution. Upvote ones you want.

**🛠️ Show and tell** — Share your setups, model pools, config recipes, and cool runs.

**🧩 Workers & providers** — Tips, gotchas, and configs for specific harnesses (Claude/Codex/Grok/OpenCode/…) and API providers (OpenRouter, DeepSeek, Cloudflare, …).

**💬 General** — Anything about Rinne that doesn't fit the other categories.
