# Use OpenAI ChatGPT OAuth

`quorum` can run native LLM agents through a ChatGPT/Codex subscription without
an `OPENAI_API_KEY`.

Use the interactive initializer and choose the OpenAI OAuth subscription
provider:

```sh
quorum init
```

During the provider step, select `OpenAI OAuth`. If no local token exists, init
prints a browser URL and a user code. After approval, credentials are stored at
`~/.nsed/openai-oauth.json` with private file permissions, and init writes the
provider into `config/agent.yml`.

The generated provider looks like this:

```yaml
providers:
  openai_oauth:
    type: openai-oauth

agents:
  - name: builder
    provider_id: openai_oauth
    model_name: gpt-5.5
```

`type: openai-oauth` uses the Codex Responses backend
(`https://chatgpt.com/backend-api/codex`) and automatically refreshes OAuth
tokens. Set `QUORUM_CODEX_BASE_URL` only when testing against a compatible
backend.

This provider does not spawn the `codex` CLI. It is a built-in, keyless
`ProviderFactory` for ChatGPT OAuth accounts, useful when the agent should
use a subscription-backed `gpt-5.5` model instead of an `OPENAI_API_KEY`.

For non-interactive fleet templates, `quorum init --agent-fleet` includes a
commented `openai_oauth` provider block. Use it only when
`~/.nsed/openai-oauth.json` already exists; otherwise prefer interactive
`quorum init` so the device login runs during setup.

To reset local OpenAI OAuth auth, remove both the current credential file and
the legacy Codex OAuth path if it exists:

```sh
rm -f ~/.nsed/openai-oauth.json ~/.nsed/openai-codex.json
```
