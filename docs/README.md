# Moul docs

Moul is a backend that runs in a browser tab: a database, access rules, sign-in, files and live
queries, kept as one SQLite file in the browser and synced, if you want, to a bucket you own as a
Litestream replica.

| Guide | For |
| --- | --- |
| [Getting started](getting-started.md) | Trying Moul in the Playground, then using it from your own app |
| [The SDK](sdk.md) | `@moul-dev/client`: records, live queries, sign-in, files, the database file, sync |
| [Rules](rules.md) | Who may list, view, create, update and delete records |
| [Studio](studio.md) | The admin console, and putting it inside your app as `<moul-studio>` |
| [Sync](sync.md) | Your other devices through a bucket: setup, hand-off, conflicts, restore |
| [Moul Hub](hub.md) | Sync without a bucket: devices sign in with your email; the hosted beta; the protocol |
| [The server](server.md) | `moul serve`: the API over HTTP, live events, mail, OAuth, webhooks, jobs, MCP for agents |
| [Testing](testing.md) | Tests without a browser, and `moul check` in CI |
| [The `moul` command](../crates/moul-cli/README.md) | `serve`, `mcp`, `hub`, `login`, `check`, `schema`, `typegen`, `pull`, `restore` |

For AI tools: `moul mcp <folder>` serves a database as MCP tools, and a server
offers them at `/api/mcp` ([details](server.md#mcp-for-ai-agents)). The SDK package ships [`llms.txt`](../packages/client/llms.txt),
[`AGENTS.md`](../packages/client/AGENTS.md) and a [skill](../packages/client/skills/moul/SKILL.md).
