# Use OpenAI ChatGPT/Codex OAuth

`quorum` can run native LLM agents through a ChatGPT/Codex subscription without
an `OPENAI_API_KEY`.

Sign in once:

```sh
quorum auth openai-codex
```

The command opens a browser OAuth flow and listens for the local callback on
`127.0.0.1:1455`. If the callback does not arrive, paste the final redirect URL
back into the terminal. After approval, credentials are stored at
`~/.nsed/openai-codex.json` with private file permissions.

For the older Codex device-code flow, run:

```sh
quorum auth openai-codex --device-code
```

Use the provider from `agent.yml`:

```yaml
providers:
  openai_codex:
    type: openai-codex

agents:
  - name: builder
    provider_id: openai_codex
    model_name: gpt-5.5
```

`type: openai-codex` uses the Codex Responses backend
(`https://chatgpt.com/backend-api/codex`) and automatically refreshes OAuth
tokens. Set `QUORUM_CODEX_BASE_URL` only when testing against a compatible
backend.

This provider does not spawn the `codex` CLI. It is a built-in, keyless
`ProviderFactory` for ChatGPT/Codex OAuth accounts, useful when the agent should
use a subscription-backed `gpt-5.5` model instead of an `OPENAI_API_KEY`.

Check local auth:

```sh
quorum auth status
```
