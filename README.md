# nth

> I've asked you for the n-th time!

I wanted to tweak my coding harness more than I could, so I product engineered it myself.

Whenever I did not know what to do, I copied [opencode](https://github.com/anomalyco/opencode). So all props to them.

## Install

Every [release](https://github.com/kloki/nth/releases) ships a prebuilt binary for x86_64 Linux. To install the latest one, run its installer:

```sh
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/kloki/nth/releases/latest/download/nth-installer.sh | sh
```

## Configure

Currently nth only supports OpenAI-compatible chat completions APIs.

To configure nth:

1. Run `nth init` to write a config with every default to `~/.config/nth/config.toml`.
2. Edit the keys you want to change.
3. Run `nth config` to print the config nth uses.

nth reads API keys from environment variables, never from the config file. Each provider names its variable with `api_key_env`. The built-in OpenCode Go provider reads `OPENCODE_API_KEY`. To set it, add it to your `.bashrc`:

```sh
export OPENCODE_API_KEY=...
```

To add another provider, add a `[provider.<id>]` block with a `base_url` and an `api_key_env`:

```toml
[provider.acme]
base_url = "https://api.acme.technology/api/"
api_key_env = "ACME_API_KEY"
```

nth leaves out a provider whose key is not set, and needs at least one that is.
