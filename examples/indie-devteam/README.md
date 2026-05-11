# indie-devteam

A three-agent code review sandbox: `senior-reviewer`, `security-auditor`,
`pm-planner`.

Each agent reads the same input code through its own lens, in its own OS
process, and writes its own markdown report. The runtime aggregates them into
`output/report.md` and copies that file to the host folder declared in
`mounts:` (defaults to `~/Desktop/indie-devteam/`).

## Run

```bash
export ANTHROPIC_API_KEY=sk-ant-...

dockagents publish ./examples/indie-devteam
dockagents install indie-devteam
dockagents run indie-devteam --input ./path/to/your/code
```

## Customize the LLM

Each agent's `llm:` block accepts:

```yaml
llm:
  provider:    anthropic        # or openai, openai-compatible
  endpoint:    https://api.anthropic.com/v1/messages
  api_key_env: ANTHROPIC_API_KEY
  api_version: "2023-06-01"
  max_tokens:  4096
  extra_headers:
    HTTP-Referer: https://my.app
```

Mix providers per agent if you want — e.g. run `senior-reviewer` on Claude and
`pm-planner` on a local OpenAI-compatible endpoint.
