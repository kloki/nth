# nth

> I've asked you for the n-th time!

I wanted to tweak my coding harness more than I could, so I product engineered it myself.

Whenever I did not know what to do, I copied [opencode](https://github.com/anomalyco/opencode). So all props to them.

## Install

Every [release](https://github.com/kloki/nth/releases) ships prebuilt binaries for macOS (Apple Silicon and Intel) and Linux (x86_64 and aarch64, glibc and musl). To install the latest one, run its installer:

```sh
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/kloki/nth/releases/latest/download/nth-installer.sh | sh
```

## Set up

nth comes with no providers, so it needs a config before it can chat.

1. Run `nth init`. It writes `~/.config/nth/config.toml` with every key, its default and what it does.
2. Uncomment a provider. OpenCode Go and OpenCode Zen are ready to go, or add your own `[provider.<id>]` block for any OpenAI-compatible endpoint.
3. Export the API key in the variable that provider's `api_key_env` names. nth reads keys only from the environment, never from the config file:

   ```sh
   export OPENCODE_API_KEY=...
   ```

4. Run `nth models` to see what the provider serves, and set `model` to one of them as `provider/model`. Leave it empty and nth picks one at random.

`nth config` prints the config nth actually uses. The template that `nth init` writes, [`crates/nth/src/config.toml`](crates/nth/src/config.toml), is the reference for every key.

## Use

```sh
nth                       # open the chat
nth -c                    # continue the last session (/resume in the chat picks any)
nth run "fix the tests"   # one prompt, headless, in act mode
nth run --mode plan "..." # only write a plan
nth --model zen/<model>   # any command, on another model (or NTH_MODEL)
```

In the chat, Tab switches between plan and act mode. Each mode can have its own model and effort under `[mode]`.

To see what nth finds for the working directory, run `nth skills`, `nth agents`, `nth formatters` or `nth lsp`. `nth notify` sends a sample desktop notification.
